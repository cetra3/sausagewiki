#![recursion_limit="128"]

#[macro_use] extern crate quote;

extern crate base64;
extern crate proc_macro;
extern crate sha2;
extern crate syn;

use proc_macro::TokenStream;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

fn user_crate_root() -> PathBuf {
    std::env::current_dir().expect("Unable to get current directory")
}

fn find_attr<'a>(attrs: &'a Vec<syn::Attribute>, name: &str) -> Option<&'a str> {
    attrs.iter()
        .find(|&x| x.name() == name)
        .and_then(|ref attr| match &attr.value {
            &syn::MetaItem::NameValue(_, syn::Lit::Str(ref template, _)) => Some(template),
            _ => None
        })
        .map(|x| x.as_ref())
}

fn buf_file<P: AsRef<Path>>(filename: P) -> Vec<u8> {
    let mut f = File::open(filename)
        .expect("Unable to open file for reading");

    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .expect("Unable to read file");

    buf
}

fn calculate_checksum<P: AsRef<Path>>(filename: P) -> String {
    use base64::*;
    use sha2::{Sha256, Digest};

    encode_config(&Sha256::digest(&buf_file(filename)), URL_SAFE)
}

#[proc_macro_derive(StaticResource, attributes(filename, mime))]
pub fn static_resource(input: TokenStream) -> TokenStream {
    let s = input.to_string();
    let ast = syn::parse_macro_input(&s).unwrap();

    let filename = find_attr(&ast.attrs, "filename")
        .expect("The `filename` attribute must be specified");
    let abs_filename = user_crate_root().join(filename);
    let abs_filename = abs_filename.to_str().expect("Absolute file path must be valid Unicode");

    let checksum = calculate_checksum(&abs_filename);

    let mime = find_attr(&ast.attrs, "mime")
        .expect("The `mime` attribute must be specified");

    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let gen = quote! {
        #[allow(unused_attributes, unused_qualifications, unknown_lints, clippy)]
        #[automatically_derived]
        impl #impl_generics Resource for #name #ty_generics #where_clause {
            fn allow(&self) -> Vec<::hyper::Method> {
                use ::hyper::Method::*;
                vec![Options, Head, Get]
            }

            fn head(&self) -> futures::BoxFuture<Response, Box<::std::error::Error + Send + Sync>> {
                futures::finished(Response::new()
                    .with_status(::hyper::StatusCode::Ok)
                    .with_header(::hyper::header::ContentType(
                        #mime.parse().expect("Statically supplied mime type must be parseable")))
                    .with_header(::hyper::header::CacheControl(vec![
                        ::hyper::header::CacheDirective::Extension("immutable".to_owned(), None),
                        ::hyper::header::CacheDirective::MaxAge(31556926),
                        ::hyper::header::CacheDirective::Public,
                    ]))
                    .with_header(::hyper::header::ETag(Self::etag()))
                ).boxed()
            }

            fn get(self: Box<Self>) -> futures::BoxFuture<Response, Box<::std::error::Error + Send + Sync>> {
                let body = include_bytes!(#abs_filename);

                self.head().map(move |head|
                    head
                        .with_header(::hyper::header::ContentLength(body.len() as u64))
                        .with_body(body as &'static [u8])
                ).boxed()
            }

            fn put(self: Box<Self>, _body: hyper::Body) -> futures::BoxFuture<Response, Box<::std::error::Error + Send + Sync>> {
                futures::finished(self.method_not_allowed()).boxed()
            }
        }

        impl #impl_generics #name #ty_generics #where_clause {
            fn checksum() -> &'static str {
                #checksum
            }

            fn etag() -> ::hyper::header::EntityTag {
                ::hyper::header::EntityTag::new(false, Self::checksum().to_owned())
            }
        }
    };

    gen.parse().unwrap()
}