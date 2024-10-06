// Copyright 2024 Oxide Computer Company

//! Core implementation for the progenitor OpenAPI client generator.

#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap, HashSet};

use openapiv3::OpenAPI;
use proc_macro2::{TokenStream, TokenTree};
use quote::quote;
use serde::Deserialize;
use thiserror::Error;
use typify::{TypeSpace, TypeSpaceSettings, TypeId};

use crate::to_schema::ToSchema;

pub use typify::CrateVers;
pub use typify::TypeSpaceImpl as TypeImpl;
pub use typify::TypeSpacePatch as TypePatch;
pub use typify::UnknownPolicy;

mod cli;
mod httpmock;
mod method;
mod template;
mod to_schema;
mod util;

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("unexpected value type {0}: {1}")]
    BadValue(String, serde_json::Value),
    #[error("type error {0}")]
    TypeError(#[from] typify::Error),
    #[error("unexpected or unhandled format in the OpenAPI document {0}")]
    UnexpectedFormat(String),
    #[error("invalid operation path {0}")]
    InvalidPath(String),
    #[error("invalid dropshot extension use: {0}")]
    InvalidExtension(String),
    #[error("internal error {0}")]
    InternalError(String),
}

#[allow(missing_docs)]
pub type Result<T> = std::result::Result<T, Error>;

/// OpenAPI generator.
pub struct Generator {
    type_space: TypeSpace,
    settings: GenerationSettings,
    uses_futures: bool,
    uses_websockets: bool,
}

/// Settings for [Generator].
#[derive(Default, Clone)]
pub struct GenerationSettings {
    interface: InterfaceStyle,
    tag: TagStyle,
    inner_type: Option<TokenStream>,
    pre_hook: Option<TokenStream>,
    pre_hook_async: Option<TokenStream>,
    post_hook: Option<TokenStream>,
    extra_derives: Vec<String>,

    unknown_crates: UnknownPolicy,
    crates: BTreeMap<String, CrateSpec>,

    patch: HashMap<String, TypePatch>,
    replace: HashMap<String, (String, Vec<TypeImpl>)>,
    convert: Vec<(schemars::schema::SchemaObject, String, Vec<TypeImpl>)>,
}

#[derive(Debug, Clone)]
struct CrateSpec {
    version: CrateVers,
    rename: Option<String>,
}

/// Style of generated client.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub enum InterfaceStyle {
    /// Use positional style.
    Positional,
    /// Use builder style.
    Builder,
}

impl Default for InterfaceStyle {
    fn default() -> Self {
        Self::Positional
    }
}

/// Style for using the OpenAPI tags when generating names in the client.
#[derive(Clone, Deserialize)]
pub enum TagStyle {
    /// Merge tags to create names in the generated client.
    Merged,
    /// Use each tag name to create separate names in the generated client.
    Separate,
}

impl Default for TagStyle {
    fn default() -> Self {
        Self::Merged
    }
}

impl GenerationSettings {
    /// Create new generator settings with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the [InterfaceStyle].
    pub fn with_interface(&mut self, interface: InterfaceStyle) -> &mut Self {
        self.interface = interface;
        self
    }

    /// Set the [TagStyle].
    pub fn with_tag(&mut self, tag: TagStyle) -> &mut Self {
        self.tag = tag;
        self
    }

    /// Client inner type available to pre and post hooks.
    pub fn with_inner_type(&mut self, inner_type: TokenStream) -> &mut Self {
        self.inner_type = Some(inner_type);
        self
    }

    /// Hook invoked before issuing the HTTP request.
    pub fn with_pre_hook(&mut self, pre_hook: TokenStream) -> &mut Self {
        self.pre_hook = Some(pre_hook);
        self
    }

    /// Hook invoked before issuing the HTTP request.
    pub fn with_pre_hook_async(&mut self, pre_hook: TokenStream) -> &mut Self {
        self.pre_hook_async = Some(pre_hook);
        self
    }

    /// Hook invoked prior to receiving the HTTP response.
    pub fn with_post_hook(&mut self, post_hook: TokenStream) -> &mut Self {
        self.post_hook = Some(post_hook);
        self
    }

    /// Additional derive macros applied to generated types.
    pub fn with_derive(&mut self, derive: impl ToString) -> &mut Self {
        self.extra_derives.push(derive.to_string());
        self
    }

    /// Modify a type with the given name.
    /// See [typify::TypeSpaceSettings::with_patch].
    pub fn with_patch<S: AsRef<str>>(
        &mut self,
        type_name: S,
        patch: &TypePatch,
    ) -> &mut Self {
        self.patch
            .insert(type_name.as_ref().to_string(), patch.clone());
        self
    }

    /// Replace a referenced type with a named type.
    /// See [typify::TypeSpaceSettings::with_replacement].
    pub fn with_replacement<
        TS: ToString,
        RS: ToString,
        I: Iterator<Item = TypeImpl>,
    >(
        &mut self,
        type_name: TS,
        replace_name: RS,
        impls: I,
    ) -> &mut Self {
        self.replace.insert(
            type_name.to_string(),
            (replace_name.to_string(), impls.collect()),
        );
        self
    }

    /// Replace a given schema with a named type.
    /// See [typify::TypeSpaceSettings::with_conversion].
    pub fn with_conversion<S: ToString, I: Iterator<Item = TypeImpl>>(
        &mut self,
        schema: schemars::schema::SchemaObject,
        type_name: S,
        impls: I,
    ) -> &mut Self {
        self.convert
            .push((schema, type_name.to_string(), impls.collect()));
        self
    }

    /// Policy regarding crates referenced by the schema extension
    /// `x-rust-type` not explicitly specified via [Self::with_crate].
    /// See [typify::TypeSpaceSettings::with_unknown_crates].
    pub fn with_unknown_crates(&mut self, policy: UnknownPolicy) -> &mut Self {
        self.unknown_crates = policy;
        self
    }

    /// Explicitly named crates whose types may be used during generation
    /// rather than generating new types based on their schemas (base on the
    /// presence of the x-rust-type extension).
    /// See [typify::TypeSpaceSettings::with_crate].
    pub fn with_crate<S1: ToString>(
        &mut self,
        crate_name: S1,
        version: CrateVers,
        rename: Option<&String>,
    ) -> &mut Self {
        self.crates.insert(
            crate_name.to_string(),
            CrateSpec {
                version,
                rename: rename.cloned(),
            },
        );
        self
    }
}

impl Default for Generator {
    fn default() -> Self {
        Self {
            type_space: TypeSpace::new(
                TypeSpaceSettings::default().with_type_mod("types"),
            ),
            settings: Default::default(),
            uses_futures: Default::default(),
            uses_websockets: Default::default(),
        }
    }
}


use schemars::schema::{InstanceType, SchemaObject};

impl Generator {
    /// Create a new generator with default values.
    pub fn new(settings: &GenerationSettings) -> Self {
        let mut type_settings = TypeSpaceSettings::default();
        type_settings
            .with_type_mod("types")
            .with_conversion(
                SchemaObject {
                    instance_type: Some(InstanceType::String.into()),
                    format: Some("binary".to_string()),
                    ..Default::default()
                },
                "Vec<u8>",
                [TypeImpl::Display].into_iter(),
            )
            .with_struct_builder(settings.interface == InterfaceStyle::Builder);
        settings.extra_derives.iter().for_each(|derive| {
            let _ = type_settings.with_derive(derive.clone());
        });

        // Control use of crates found in x-rust-type extension
        type_settings.with_unknown_crates(settings.unknown_crates);
        settings.crates.iter().for_each(
            |(crate_name, CrateSpec { version, rename })| {
                type_settings.with_crate(
                    crate_name,
                    version.clone(),
                    rename.as_ref(),
                );
            },
        );

        // Adjust generation by type, name, or schema.
        println!("About to patch");
        settings.patch.iter().for_each(|(type_name, patch)| {
            println!("patch: {type_name} -> {patch:?}");
            type_settings.with_patch(type_name, patch);
        });
        settings.replace.iter().for_each(
            |(type_name, (replace_name, impls))| {
                type_settings.with_replacement(
                    type_name,
                    replace_name,
                    impls.iter().cloned(),
                );
            },
        );
        settings
            .convert
            .iter()
            .for_each(|(schema, type_name, impls)| {
                type_settings.with_conversion(
                    schema.clone(),
                    type_name,
                    impls.iter().cloned(),
                );
            });

        Self {
            type_space: TypeSpace::new(&type_settings),
            settings: settings.clone(),
            uses_futures: false,
            uses_websockets: false,
        }
    }

    /// Emit a [TokenStream] containing the generated client code.
    pub fn generate_tokens(&mut self, spec: &OpenAPI) -> Result<TokenStream> {
        validate_openapi(spec)?;

        // Convert our components dictionary to schemars
        let schemas = spec.components.iter().flat_map(|components| {
            components.schemas.iter().map(|(name, ref_or_schema)| {
                (name.clone(), ref_or_schema.to_schema())
            })
        });

        self.type_space.add_ref_types(schemas)?;

        let raw_methods = spec
            .paths
            .iter()
            .flat_map(|(path, ref_or_item)| {
                // Exclude externally defined path items.
                let item = ref_or_item.as_item().unwrap();
                item.iter().map(move |(method, operation)| {
                    (path.as_str(), method, operation, &item.parameters)
                })
            })
            .map(|(path, method, operation, path_parameters)| {
                self.process_operation(
                    operation,
                    &spec.components,
                    path,
                    method,
                    path_parameters,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let encodings = spec
            .paths
            .iter()
            .flat_map(|(_, ref_or_item)| ref_or_item.as_item())
            .flat_map(|item| item.iter())
            .filter_map(|(operation_id, operation)| operation.request_body.as_ref())
            .flat_map(|request_body| request_body.as_item())
            .filter_map(|body| body.content.get("multipart/form-data"))
            .flat_map(|content| {
                // println!("content:\n {:?}\n\n", content);
                // println!("encoding:\n {:?}\n\n", content.encoding);
                content.encoding.iter().filter_map(|(name, encoding)| {
                    if let Some(ctype) = &encoding.content_type {
                        Some((name.to_string(), ctype.as_str()))
                        // match ctype.as_str() {
                        //     "application/x-protobuf" | "application/octet-stream" => {
                        //         // binary types
                        //         Some((name.to_string(), true))
                        //     }
                        //     _ => Some((name.to_string(), false)),
                        // }
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();


        println!("aaa: {:?}\n", encodings);


        // use std::fmt::Debug;
        // // Generate code for multipart forms
        // let multipart_form = method
        // .params
        // .iter()
        // .filter_map(|param| match &param.kind {
        //     OperationParameterKind::Body(BodyContentType::MultipartFormData) => {
        //         match &param.typ {
        //             OperationParameterType::Type(type_id) => {
        //                 let ty = self.type_space.get_type(type_id).ok()?;
        //                 match ty.builder() {
        //                     Some(a) => {
        //                         println!("a: {:?}", a.clone());
        //                         let _ = a.clone()
        //                         .into_iter()
        //                         .filter(|c| {
        //                             println!("c: {}", c);
        //                             true
        //                         });
        //                         let res = quote! {
        //                             let #a;
        //                         };
        //                         Some(res)
        //                     },
        //                     None => None,
        //                 }
        //             },
        //            &OperationParameterType::RawBody => todo!()
        //         }
        //     }
        //     _ => None,
        // })
        // .collect::<Vec<_>>();



        // Get list of multipart form data parts
        // let multipart_form_parts = spec
        //     .paths
        //     .iter()
        //     .flat_map(|(_, path_item_ref)| path_item_ref.as_item())
        //     .flat_map(|item| item.iter())  //.filter_map(|(_, operation)| operation.request_body.as_ref())
        //     .filter_map(|(_, operation)|
        //         match operation.request_body.as_ref() {
        //             Some(request_body_item) => {
        //                 match operation.operation_id.as_ref() {
        //                     Some(operation_id) => {
        //                         let operation_id_body = format!("{}Body", operation_id);
        //                         let capitalized_operation_id_body = operation_id_body[..1].to_uppercase() + &operation_id_body[1..];
        //                         Some((capitalized_operation_id_body, request_body_item))
        //                     }
        //                     None => None,
        //                 }
        //             },
        //             None => None,
        //         }
        //     )
        //     .filter_map(|(operation_id, request_body_item)|
        //         match request_body_item.as_item() {
        //             Some(request_body) => Some((operation_id, request_body)),
        //             None => None,
        //         }
        //     )
        //     .filter_map(|(operation_id, body)|
        //         match body.content.get("multipart/form-data") {
        //             Some(media_type) => Some((operation_id, media_type)),
        //             None => None,
        //         }
        //     )
        //     .filter_map(|(operation_id, media_type)|
        //         match media_type.schema.as_ref(){
        //             Some(schema_item) => Some((operation_id, schema_item)),
        //             None => None,
        //         }
        //     )
        //     .flat_map(|(operation_id, schema_item)| 
        //         match schema_item.as_item() {
        //                 Some(schema) => Some((operation_id, schema)),
        //                 None => None,
        //             }
        //     )
        //     .flat_map(|(operation_id, schema)| 
        //         match schema {
        //             openapiv3::Schema {
        //                 schema_kind: openapiv3::SchemaKind::Type(openapiv3::Type::Object(schema_kind)),
        //                 ..
        //             } => Some((operation_id.clone(), &schema_kind.properties)),
        //             _ => None,
        //         }
        //     )
        //     .flat_map(|(operation_id, properties)|
        //         properties.iter().map(move |(name, property)| (operation_id.clone(), name, property))
        //     )
        //     .filter_map(|(operation_id, name, property)|
        //         match property.as_item() {
        //             Some(p) => Some((operation_id, name.to_string(), p.schema_kind.clone())),
        //             _ => None,
        //         }
        //     )
        //     .flat_map(|(operation_id, name, schema_kind)|
        //         match schema_kind {
        //             openapiv3::SchemaKind::Type(openapiv3::Type::String(string_schema)) => Some((operation_id, name, string_schema.clone())),
        //             _ => None,
        //         }
        //     )
        //     .filter_map(|(operation_id, name, string_schema)|
        //         match string_schema.format {
        //             openapiv3::VariantOrUnknownOrEmpty::Item(format) => Some((operation_id, name, Some(format.clone()))),
        //             _ => Some((operation_id, name, None)),
        //         }
        //     )
        //     .filter_map(|(operation_id, name, format)| {
        //         match format {
        //             Some(openapiv3::StringFormat::Binary) => Some((operation_id, name, true)),
        //             _ => Some((operation_id, name, false)),
        //         }
        //     })
        //     .collect::<Vec<_>>();

        // for schema_property in multipart_form_parts {
        //     println!("multipart_form part:\n{:?}\n", schema_property);
        // }

        let multipart_form = process_multipart_form(&spec);

        // impl UploadProtobufsBody {
        //     // Create a multipart form for reqwest
        //     pub fn into_multipart(self) -> Result<reqwest::multipart::Form, reqwest::Error> {
        //         let mut form = reqwest::multipart::Form::new();
        //         let metadata = reqwest::multipart::Part::bytes(self.metadata.clone())
        //             .file_name("metadata")
        //             .mime_str("application/x-protobuf")?;
        //         form = form.part("metadata", metadata);
        //         let payload = reqwest::multipart::Part::bytes(self.payload.clone())
        //             .file_name("payload")
        //             .mime_str("application/octet-stream")?;
        //         form = form.part("payload", payload);
        //         Ok(form)
        //     }
        // }

        let operation_code = match (
            &self.settings.interface,
            &self.settings.tag,
        ) {
            (InterfaceStyle::Positional, TagStyle::Merged) => {
                self.generate_tokens_positional_merged(&raw_methods)
            }
            (InterfaceStyle::Positional, TagStyle::Separate) => {
                unimplemented!("positional arguments with separate tags are currently unsupported")
            }
            (InterfaceStyle::Builder, TagStyle::Merged) => {
                self.generate_tokens_builder_merged(&raw_methods)
            }
            (InterfaceStyle::Builder, TagStyle::Separate) => {
                let tag_info = spec
                    .tags
                    .iter()
                    .map(|tag| (&tag.name, tag))
                    .collect::<BTreeMap<_, _>>();
                self.generate_tokens_builder_separate(&raw_methods, tag_info)
            }
        }?;

        let types = self.type_space.to_stream();

        // Generate an implementation of a `Self::as_inner` method, if an inner
        // type is defined.
        let maybe_inner = self.settings.inner_type.as_ref().map(|inner| {
            quote! {
                /// Return a reference to the inner type stored in `self`.
                pub fn inner(&self) -> &#inner {
                    &self.inner
                }
            }
        });

        let inner_property = self.settings.inner_type.as_ref().map(|inner| {
            quote! {
                pub (crate) inner: #inner,
            }
        });
        let inner_parameter = self.settings.inner_type.as_ref().map(|inner| {
            quote! {
                inner: #inner,
            }
        });
        let inner_value = self.settings.inner_type.as_ref().map(|_| {
            quote! {
                inner
            }
        });

        let client_docstring = {
            let mut s = format!("Client for {}", spec.info.title);

            if let Some(ss) = &spec.info.description {
                s.push_str("\n\n");
                s.push_str(ss);
            }
            if let Some(ss) = &spec.info.terms_of_service {
                s.push_str("\n\n");
                s.push_str(ss);
            }

            s.push_str(&format!("\n\nVersion: {}", &spec.info.version));

            s
        };

        let version_str = &spec.info.version;

        // The allow(unused_imports) on the `pub use` is necessary with Rust 1.76+, in case the
        // generated file is not at the top level of the crate.

        let file = quote! {
            // Re-export ResponseValue and Error since those are used by the
            // public interface of Client.
            #[allow(unused_imports)]
            pub use progenitor_client::{ByteStream, Error, ResponseValue};
            #[allow(unused_imports)]
            use progenitor_client::{encode_path, RequestBuilderExt};
            #[allow(unused_imports)]
            use reqwest::header::{HeaderMap, HeaderValue};

            /// Types used as operation parameters and responses.
            #[allow(clippy::all)]
            pub mod types {
                #types

                #multipart_form
            }

            #[derive(Clone, Debug)]
            #[doc = #client_docstring]
            pub struct Client {
                pub(crate) baseurl: String,
                pub(crate) client: reqwest::Client,
                #inner_property
            }

            impl Client {
                /// Create a new client.
                ///
                /// `baseurl` is the base URL provided to the internal
                /// `reqwest::Client`, and should include a scheme and hostname,
                /// as well as port and a path stem if applicable.
                pub fn new(
                    baseurl: &str,
                    #inner_parameter
                ) -> Self {
                    #[cfg(not(target_arch = "wasm32"))]
                    let client = {
                        let dur = std::time::Duration::from_secs(15);

                        reqwest::ClientBuilder::new()
                            .connect_timeout(dur)
                            .timeout(dur)
                    };
                    #[cfg(target_arch = "wasm32")]
                    let client = reqwest::ClientBuilder::new();

                    Self::new_with_client(baseurl, client.build().unwrap(), #inner_value)
                }

                /// Construct a new client with an existing `reqwest::Client`,
                /// allowing more control over its configuration.
                ///
                /// `baseurl` is the base URL provided to the internal
                /// `reqwest::Client`, and should include a scheme and hostname,
                /// as well as port and a path stem if applicable.
                pub fn new_with_client(
                    baseurl: &str,
                    client: reqwest::Client,
                    #inner_parameter
                ) -> Self {
                    Self {
                        baseurl: baseurl.to_string(),
                        client,
                        #inner_value
                    }
                }

                /// Get the base URL to which requests are made.
                pub fn baseurl(&self) -> &String {
                    &self.baseurl
                }

                /// Get the internal `reqwest::Client` used to make requests.
                pub fn client(&self) -> &reqwest::Client {
                    &self.client
                }

                /// Get the version of this API.
                ///
                /// This string is pulled directly from the source OpenAPI
                /// document and may be in any format the API selects.
                pub fn api_version(&self) -> &'static str {
                    #version_str
                }

                #maybe_inner
            }

            #operation_code
        };

        Ok(file)
    }

    fn generate_tokens_positional_merged(
        &mut self,
        input_methods: &[method::OperationMethod],
    ) -> Result<TokenStream> {
        let methods = input_methods
            .iter()
            .map(|method| self.positional_method(method))
            .collect::<Result<Vec<_>>>()?;

        // The allow(unused_imports) on the `pub use` is necessary with Rust 1.76+, in case the
        // generated file is not at the top level of the crate.

        let out = quote! {
            #[allow(clippy::all)]
            impl Client {
                #(#methods)*
            }

            /// Items consumers will typically use such as the Client.
            pub mod prelude {
                #[allow(unused_imports)]
                pub use super::Client;
            }
        };
        Ok(out)
    }

    fn generate_tokens_builder_merged(
        &mut self,
        input_methods: &[method::OperationMethod],
    ) -> Result<TokenStream> {
        let builder_struct = input_methods
            .iter()
            .map(|method| self.builder_struct(method, TagStyle::Merged))
            .collect::<Result<Vec<_>>>()?;

        let builder_methods = input_methods
            .iter()
            .map(|method| self.builder_impl(method))
            .collect::<Vec<_>>();

        let out = quote! {
            impl Client {
                #(#builder_methods)*
            }

            /// Types for composing operation parameters.
            #[allow(clippy::all)]
            pub mod builder {
                use super::types;
                #[allow(unused_imports)]
                use super::{
                    encode_path,
                    ByteStream,
                    Error,
                    HeaderMap,
                    HeaderValue,
                    RequestBuilderExt,
                    ResponseValue,
                };

                #(#builder_struct)*
            }

            /// Items consumers will typically use such as the Client.
            pub mod prelude {
                pub use self::super::Client;
            }
        };

        Ok(out)
    }

    fn generate_tokens_builder_separate(
        &mut self,
        input_methods: &[method::OperationMethod],
        tag_info: BTreeMap<&String, &openapiv3::Tag>,
    ) -> Result<TokenStream> {
        let builder_struct = input_methods
            .iter()
            .map(|method| self.builder_struct(method, TagStyle::Separate))
            .collect::<Result<Vec<_>>>()?;

        let (traits_and_impls, trait_preludes) =
            self.builder_tags(input_methods, &tag_info);

        // The allow(unused_imports) on the `pub use` is necessary with Rust 1.76+, in case the
        // generated file is not at the top level of the crate.

        let out = quote! {
            #traits_and_impls

            /// Types for composing operation parameters.
            #[allow(clippy::all)]
            pub mod builder {
                use super::types;
                #[allow(unused_imports)]
                use super::{
                    encode_path,
                    ByteStream,
                    Error,
                    HeaderMap,
                    HeaderValue,
                    RequestBuilderExt,
                    ResponseValue,
                };

                #(#builder_struct)*
            }

            /// Items consumers will typically use such as the Client and
            /// extension traits.
            pub mod prelude {
                #[allow(unused_imports)]
                pub use super::Client;
                #trait_preludes
            }
        };

        Ok(out)
    }

    /// Get the [TypeSpace] for schemas present in the OpenAPI specification.
    pub fn get_type_space(&self) -> &TypeSpace {
        &self.type_space
    }

    /// Whether the generated client needs to use additional crates to support futures.
    pub fn uses_futures(&self) -> bool {
        self.uses_futures
    }

    /// Whether the generated client needs to use additional crates to support websockets.
    pub fn uses_websockets(&self) -> bool {
        self.uses_websockets
    }
}

/// Add newlines after end-braces at <= two levels of indentation.
pub fn space_out_items(content: String) -> Result<String> {
    Ok(if cfg!(not(windows)) {
        let regex = regex::Regex::new(r#"(\n\s*})(\n\s{0,8}[^} ])"#).unwrap();
        regex.replace_all(&content, "$1\n$2").to_string()
    } else {
        let regex = regex::Regex::new(r#"(\n\s*})(\r\n\s{0,8}[^} ])"#).unwrap();
        regex.replace_all(&content, "$1\r\n$2").to_string()
    })
}

/// Do some very basic checks of the OpenAPI documents.
pub fn validate_openapi(spec: &OpenAPI) -> Result<()> {
    match spec.openapi.as_str() {
        "3.0.0" | "3.0.1" | "3.0.2" | "3.0.3" => (),
        v => {
            return Err(Error::UnexpectedFormat(format!(
                "invalid version: {}",
                v
            )))
        }
    }

    let mut opids = HashSet::new();
    spec.paths.paths.iter().try_for_each(|p| {
        match p.1 {
            openapiv3::ReferenceOr::Reference { reference: _ } => {
                Err(Error::UnexpectedFormat(format!(
                    "path {} uses reference, unsupported",
                    p.0,
                )))
            }
            openapiv3::ReferenceOr::Item(item) => {
                // Make sure every operation has an operation ID, and that each
                // operation ID is only used once in the document.
                item.iter().try_for_each(|(_, o)| {
                    if let Some(oid) = o.operation_id.as_ref() {
                        if !opids.insert(oid.to_string()) {
                            return Err(Error::UnexpectedFormat(format!(
                                "duplicate operation ID: {}",
                                oid,
                            )));
                        }
                    } else {
                        return Err(Error::UnexpectedFormat(format!(
                            "path {} is missing operation ID",
                            p.0,
                        )));
                    }
                    Ok(())
                })
            }
        }
    })?;

    Ok(())
}

#[derive(Debug, Clone)]
struct MultiPartParts {
    /// The name of the part
    name: String,
    /// The content type of the part
    content_type: String,
    /// The binary of the part
    binary: bool,
}

impl MultiPartParts {
    fn new() -> Self {
        Self {
            name: "".to_string(),
            content_type: "".to_string(),
            binary: false,
        }
    }
}

// Process the OpenAPI document to generate the multpart form specific code
fn process_multipart_form(spec: &OpenAPI) -> TokenStream {
    let multipart_forms = spec
        .paths
        .iter()
        .flat_map(|(_, path_item_ref)| path_item_ref.as_item())
        .flat_map(|item| item.iter())  //.filter_map(|(_, operation)| operation.request_body.as_ref())
        .filter_map(|(_, operation)|
            match operation.request_body.as_ref() {
                Some(request_body_item) => {
                    match operation.operation_id.as_ref() {
                        Some(operation_id) => {
                            let operation_id_body = format!("{}Body", operation_id);
                            let capitalized_operation_id_body = operation_id_body[..1].to_uppercase() + &operation_id_body[1..];
                            Some((capitalized_operation_id_body, request_body_item))
                        }
                        None => None,
                    }
                },
                None => None,
            }
        )
        .filter_map(|(operation_id, request_body_item)|
            match request_body_item.as_item() {
                Some(request_body) => Some((operation_id, request_body)),
                None => None,
            }
        )
        .filter_map(|(operation_id, body)|
            match body.content.get("multipart/form-data") {
                Some(media_type) => Some((operation_id, media_type)),
                None => None,
            }
        )
        .collect::<Vec<_>>();

        let mut multipart_form_parts = std::collections::HashMap::new();
        for (operation_id, media_type) in &multipart_forms {
            match media_type.schema.as_ref() {
                None => continue,
                Some(schema_item) => {
                    match schema_item.as_item() {
                        None => continue,
                        Some(schema) => {
                            match schema {
                                openapiv3::Schema {
                                    schema_kind: openapiv3::SchemaKind::Type(openapiv3::Type::Object(schema_kind)),
                                    ..
                                } => {
                                    for (name, property) in schema_kind.properties.iter() {
                                        multipart_form_parts
                                            .entry(operation_id.clone())
                                            .or_insert_with(HashMap::new)
                                            .insert(name.to_string(), HashMap::new());
                                        match property.as_item() {
                                            None => {},
                                            Some(property) => {
                                                match &property.as_ref().schema_kind {
                                                    openapiv3::SchemaKind::Type(openapiv3::Type::String(string_schema)) => {
                                                        match string_schema.format {
                                                            openapiv3::VariantOrUnknownOrEmpty::Item(format) => {
                                                                match format {
                                                                    openapiv3::StringFormat::Binary => {
                                                                        multipart_form_parts
                                                                            .entry(operation_id.clone())
                                                                            .or_insert_with(HashMap::new)
                                                                            .entry(name.to_string())
                                                                            .or_insert_with(HashMap::new)
                                                                            .insert("parttype","bytes".to_string());
                                                                    },
                                                                    _ => {
                                                                        multipart_form_parts
                                                                            .entry(operation_id.clone())
                                                                            .or_insert_with(HashMap::new)
                                                                            .entry(name.to_string())
                                                                            .or_insert_with(HashMap::new)
                                                                            .insert("parttype","text".to_string());
                                                                    },
                                                                }
                                                            },
                                                            _ => {},
                                                        }
                                                    },
                                                    _ => {},
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {},
                            }
                        }
                    }
                }
            }
        }

        for (operation_id, media_type) in multipart_forms {
            for (name, encoding) in media_type.encoding.iter() {
                match &encoding.content_type {
                    Some(content_type) => {
                        multipart_form_parts
                            .entry(operation_id.clone())
                            .or_insert_with(HashMap::new)
                            .entry(name.to_string())
                            .or_insert_with(HashMap::new)
                            .insert("content_type", content_type.to_string());
                    },
                    None => {},
                }
            }
        }

    // for (operation_id, properties) in multipart_forms {
    //     let property_list = properties.iter()
    //         .filter_map(move |(name, property)| 
    //             match property.as_item() {
    //                 Some(p) => Some((name.to_string(), p.schema_kind.clone())),
    //                 _ => None,
    //             }
    //         )
    //         .flat_map(|(name, schema_kind)|
    //             match schema_kind {
    //                 openapiv3::SchemaKind::Type(openapiv3::Type::String(string_schema)) => Some((name, string_schema.clone())),
    //                 _ => None,
    //             }
    //         )
    //         .filter_map(|(name, string_schema)|
    //             match string_schema.format {
    //                 openapiv3::VariantOrUnknownOrEmpty::Item(format) => Some((name, Some(format.clone()))),
    //                 _ => Some((name, None)),
    //             }
    //         )
    //         .filter_map(|(name, format)|
    //             match format {
    //                 Some(openapiv3::StringFormat::Binary) => Some((name, true)),
    //                 _ => Some((name, false)),
    //             }
    //         )
    //         .collect::<Vec<_>>();

    //     multipart_form_parts.insert(operation_id, property_list);
    // }
    println!("multipart_form_parts:\n {:?}\n", multipart_form_parts);

    let into_multipart_list = multipart_form_parts.iter()
        .flat_map(move |(operation_id, property_list)| {
            let parts = property_list.iter().map(move |(partname, properties)| {
                // for (key, value) in properties.iter() {
                //     println!("-partname: {:?}", partname.clone());
                //     println!("-key: {:?}", key.clone());
                //     println!("-value: {:?}", value.clone());
                // }
                let parttype: TokenStream = properties.get("parttype").unwrap().parse().unwrap();
                let ppartname: TokenStream = partname.clone().parse().unwrap();

                let content_type = properties.get("content_type").unwrap();
                    quote! {
                        let #ppartname = reqwest::multipart::Part::#parttype(self.#ppartname.clone())
                            .mime_str(#content_type)?;
                        form = form.part(#partname, #ppartname);
                    }
                })
                .collect::<Vec<_>>();
            let op_id: TokenStream = operation_id.clone().parse().unwrap();
            quote! {
            
                impl #op_id {
            
                    pub fn into_multipart(self) -> Result<reqwest::multipart::Form, reqwest::Error> {
                        let mut form = reqwest::multipart::Form::new();
                        #(#parts)*
                        Ok(form)
                    }
                }
            }
        })
        .collect::<Vec<_>>();
    

    println!("into_multipart_list:\n {:?}\n", into_multipart_list);
    return quote! {
        #(#into_multipart_list)*
    };
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Error;

    #[test]
    fn test_bad_value() {
        assert_eq!(
            Error::BadValue("nope".to_string(), json! { "nope"},).to_string(),
            "unexpected value type nope: \"nope\"",
        );
    }

    #[test]
    fn test_type_error() {
        assert_eq!(
            Error::UnexpectedFormat("nope".to_string()).to_string(),
            "unexpected or unhandled format in the OpenAPI document nope",
        );
    }

    #[test]
    fn test_invalid_path() {
        assert_eq!(
            Error::InvalidPath("nope".to_string()).to_string(),
            "invalid operation path nope",
        );
    }

    #[test]
    fn test_internal_error() {
        assert_eq!(
            Error::InternalError("nope".to_string()).to_string(),
            "internal error nope",
        );
    }
}
