use indexmap::IndexMap;
use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    parse_quote,
    punctuated::Punctuated,
    spanned::Spanned,
    token::{Brace, Paren, PathSep},
    FieldPat, Fields, ItemEnum, LitBool, Member, Pat, PatIdent, PatPath, PatRest, PatStruct,
    PatTupleStruct, PatWild, Path, PathArguments, PathSegment, Token, Variant,
};

use proc_macro_crate::{crate_name, FoundCrate};

pub(crate) mod kw {
    // Variant fatality is determined based on the inner value, if there is only one, if multiple, the first is chosen.
    syn::custom_keyword!(forward);
    // Scrape the `thiserror` `transparent` annotation.
    syn::custom_keyword!(transparent);
    // Enum annotation to be splitable.
    syn::custom_keyword!(splitable);
    // Expand a particular annotation and only that.
    syn::custom_keyword!(expand);
}

#[derive(Clone)]
pub(crate) enum ResolutionMode {
    /// Not relevant for fatality determination, always non-fatal.
    NoAnnotation,
    /// Fatal by default.
    Fatal,
    /// Specified via a `bool` argument `#[fatal(true)]` or `#[fatal(false)]`.
    WithExplicitBool(LitBool),
    /// Specified via a keyword argument `#[fatal(forward)]`.
    Forward(kw::forward, Option<Ident>),
}

impl std::fmt::Debug for ResolutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnnotation => writeln!(f, "None"),
            Self::Fatal => writeln!(f, "Fatal"),
            Self::WithExplicitBool(ref b) => writeln!(f, "Fatal({})", b.value()),
            Self::Forward(_, ref ident) => writeln!(
                f,
                "Fatal(Forward, {})",
                ident
                    .as_ref()
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "___".to_string())
            ),
        }
    }
}

impl Default for ResolutionMode {
    fn default() -> Self {
        Self::Fatal
    }
}

impl Parse for ResolutionMode {
    fn parse(input: ParseStream) -> Result<Self, syn::Error> {
        let content = dbg!(input);

        let lookahead = content.lookahead1();

        if lookahead.peek(kw::forward) {
            Ok(Self::Forward(content.parse::<kw::forward>()?, None))
        } else if lookahead.peek(LitBool) {
            Ok(Self::WithExplicitBool(content.parse::<LitBool>()?))
        } else {
            Err(lookahead.error())
        }
    }
}

impl ToTokens for ResolutionMode {
    fn to_tokens(&self, ts: &mut TokenStream) {
        let trait_fatality = abs_helper_path(format_ident!("Fatality"), Span::call_site());
        let tmp = match self {
            Self::NoAnnotation => quote! { false },
            Self::Fatal => quote! { true },
            Self::WithExplicitBool(boolean) => {
                let value = boolean.value;
                quote! { #value }
            }
            Self::Forward(_, maybe_ident) => {
                let ident = maybe_ident
                    .as_ref()
                    .expect("Forward must have ident set. qed");
                quote! {
                    <_ as #trait_fatality >::is_fatal( #ident )
                }
            }
        };
        ts.extend(tmp)
    }
}

fn abs_helper_path(what: impl Into<Path>, loco: Span) -> Path {
    let what = what.into();
    let found_crate = if cfg!(test) {
        FoundCrate::Itself
    } else {
        crate_name("fatality").expect("`fatality` must be present in `Cargo.toml` for use. q.e.d")
    };
    let path: Path = match found_crate {
        FoundCrate::Itself => parse_quote!( crate::#what ),
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, loco);
            parse_quote! { :: #ident :: #what }
        }
    };
    path
}

/// Implement `trait Fatality` for `who`.
fn trait_fatality_impl_for_enum(
    who: &Ident,
    pattern_lut: &IndexMap<Variant, Pat>,
    resolution_lut: &IndexMap<Variant, ResolutionMode>,
) -> TokenStream {
    let pat = pattern_lut.values();
    let resolution = resolution_lut.values();

    let fatality_trait = abs_helper_path(Ident::new("Fatality", who.span()), who.span());
    quote! {
        impl #fatality_trait for #who {
            fn is_fatal(&self) -> bool {
                match self {
                    #( #pat => #resolution, )*
                }
            }
        }
    }
}

/// Implement `trait Fatality` for `who`.
fn trait_fatality_impl_for_struct(who: &Ident, resolution: &ResolutionMode) -> TokenStream {
    let fatality_trait = abs_helper_path(Ident::new("Fatality", who.span()), who.span());
    let resolution = match resolution {
        ResolutionMode::Forward(_fwd, field) => {
            let field = field
                .as_ref()
                .expect("Ident must be filled at this point. qed");
            quote! {
                #fatality_trait :: is_fatal( & self. #field )
            }
        }
        rm => quote! {
            #rm
        },
    };
    quote! {
        impl #fatality_trait for #who {
            fn is_fatal(&self) -> bool {
                #resolution
            }
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Transparent(kw::transparent);

impl Parse for Transparent {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content = dbg!(input);

        let lookahead = content.lookahead1();

        if lookahead.peek(kw::transparent) {
            Ok(Self(content.parse::<kw::transparent>()?))
        } else {
            Err(lookahead.error())
        }
    }
}

/// Returns the pattern to match, and if there is an inner ident
/// that was annotated with `#[source]`, which would be used to defer
/// `is_fatal` resolution.
///
/// Consumes a requested `ResolutionMode` and returns the same mode,
/// with a populated identifier, or errors.
fn enum_variant_to_pattern(
    variant: &Variant,
    requested_resolution_mode: ResolutionMode,
) -> Result<(Pat, ResolutionMode), syn::Error> {
    to_pattern(
        &variant.ident,
        &variant.fields,
        &variant.attrs,
        requested_resolution_mode,
    )
}

fn struct_to_pattern(
    item: &syn::ItemStruct,
    requested_resolution_mode: ResolutionMode,
) -> Result<(Pat, ResolutionMode), syn::Error> {
    to_pattern(
        &item.ident,
        &item.fields,
        &item.attrs,
        requested_resolution_mode,
    )
}

fn to_pattern(
    name: &Ident,
    fields: &Fields,
    attrs: &Vec<syn::Attribute>,
    requested_resolution_mode: ResolutionMode,
) -> Result<(Pat, ResolutionMode), syn::Error> {
    let span = fields.span();
    // default name for referencing a var in an unnamed enum variant
    let me = PathSegment {
        ident: Ident::new("Self", span),
        arguments: PathArguments::None,
    };
    let path = Path {
        leading_colon: None,
        segments: Punctuated::<PathSegment, PathSep>::from_iter(vec![me, name.clone().into()]),
    };
    let is_transparent = attrs
        .iter()
        .find(|attr| {
            if attr.path().is_ident("error") {
                attr.parse_args::<Transparent>().is_ok()
            } else {
                false
            }
        })
        .is_some();

    let source = Ident::new("source", span);
    let from = Ident::new("from", span);

    let (pat, resolution) = match fields {
        Fields::Named(ref fields) => {
            let (fields, resolution) = match requested_resolution_mode {
                ResolutionMode::Forward(fwd, _ident) => {
                    let fwd_field = if is_transparent {
                        fields.named.first().ok_or_else(|| syn::Error::new(fields.span(), "Missing inner field, must have exactly one inner field type, but requires one for `#[fatal(forward)]`."))?
                    } else {
                        fields.named.iter().find(|field| {
                            field
                                .attrs
                                .iter()
                                .find(|attr| attr.path().is_ident(&source) || attr.path().is_ident(&from))
                                .is_some()
                        }).ok_or_else(|| syn::Error::new(
                            fields.span(),
                            "No field annotated with `#[source]` or `#[from]`, but requires one for `#[fatal(forward)]`.")
                        )?
                    };

                    assert!(matches!(_ident, None));

                    // let fwd_field = fwd_field.as_ref().unwrap();
                    let field_name = fwd_field
                        .ident
                        .clone()
                        .expect("Must have member/field name. qed");
                    let fp = FieldPat {
                        attrs: vec![],
                        member: Member::Named(field_name.clone()),
                        colon_token: None,
                        pat: Box::new(Pat::Ident(PatIdent {
                            attrs: vec![],
                            by_ref: Some(Token![ref](span)),
                            mutability: None,
                            ident: field_name.clone(),
                            subpat: None,
                        })),
                    };
                    (
                        Punctuated::<FieldPat, Token![,]>::from_iter([fp]),
                        ResolutionMode::Forward(fwd, fwd_field.ident.clone()),
                    )
                }
                rm => (Punctuated::<FieldPat, Token![,]>::new(), rm),
            };

            (
                Pat::Struct(PatStruct {
                    attrs: vec![],
                    path,
                    brace_token: Brace(span),
                    fields,
                    qself: None,
                    rest: Some(PatRest {
                        attrs: vec![],
                        dot2_token: Token![..](span),
                    }),
                }),
                resolution,
            )
        }
        Fields::Unnamed(ref fields) => {
            let (mut field_pats, resolution) = if let ResolutionMode::Forward(keyword, _) =
                requested_resolution_mode
            {
                // obtain the i of the i-th unnamed field.
                let fwd_idx = if is_transparent {
                    // must be the only field, otherwise bail
                    if fields.unnamed.iter().count() != 1 {
                        return Err(
							syn::Error::new(
								fields.span(),
								"Must have exactly one parameter when annotated with `#[transparent]` annotated field for `forward` with `fatality`",
							)
						);
                    }
                    0_usize
                } else {
                    fields
                        .unnamed
                        .iter()
                        .enumerate()
                        .find_map(|(idx, field)| {
                            field
                                .attrs
                                .iter()
                                .find(|attr| {
                                    attr.path().is_ident(&source) || attr.path().is_ident(&from)
                                })
                                .map(|_attr| idx)
                        })
                        .ok_or_else(|| {
                            syn::Error::new(
										span,
										"Must have a `#[source]` or `#[from]` annotated field for `#[fatal(forward)]`",
								)
                        })?
                };

                let pat_capture_ident =
                    unnamed_fields_variant_pattern_constructor_binding_name(fwd_idx);
                // create a pattern like this: `_, _, _, inner, ..`
                let mut field_pats = std::iter::repeat(Pat::Wild(PatWild {
                    attrs: vec![],
                    underscore_token: Token![_](span),
                }))
                .take(fwd_idx)
                .collect::<Vec<_>>();

                field_pats.push(Pat::Ident(PatIdent {
                    attrs: vec![],
                    by_ref: Some(Token![ref](span)),
                    mutability: None,
                    ident: pat_capture_ident.clone(),
                    subpat: None,
                }));

                (
                    field_pats,
                    ResolutionMode::Forward(keyword, Some(pat_capture_ident)),
                )
            } else {
                (vec![], requested_resolution_mode)
            };
            field_pats.push(Pat::Rest(PatRest {
                attrs: vec![],
                dot2_token: Token![..](span),
            }));
            (
                Pat::TupleStruct(PatTupleStruct {
                    attrs: vec![],
                    path,
                    qself: None,
                    paren_token: Paren(span),
                    elems: Punctuated::<Pat, Token![,]>::from_iter(field_pats),
                }),
                resolution,
            )
        }
        Fields::Unit => {
            if let ResolutionMode::Forward(..) = requested_resolution_mode {
                return Err(syn::Error::new(
                    span,
                    "Cannot forward to a unit item variant",
                ));
            }
            (
                Pat::Path(PatPath {
                    attrs: vec![],
                    qself: None,
                    path,
                }),
                requested_resolution_mode,
            )
        }
    };
    assert!(
        !matches!(resolution, ResolutionMode::Forward(_kw, None)),
        "We always set the resolution identifier _right here_. qed"
    );

    Ok((pat, resolution))
}

fn unnamed_fields_variant_pattern_constructor_binding_name(ith: usize) -> Ident {
    Ident::new(format!("arg_{}", ith).as_str(), Span::call_site())
}

#[derive(Hash, Debug)]
struct VariantPattern(Variant);

impl ToTokens for VariantPattern {
    fn to_tokens(&self, ts: &mut TokenStream) {
        let variant_name = &self.0.ident;
        let variant_fields = &self.0.fields;

        match variant_fields {
            Fields::Unit => {
                ts.extend(quote! { #variant_name });
            }
            Fields::Unnamed(unnamed) => {
                let pattern = unnamed
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(ith, _field)| {
                        Pat::Ident(PatIdent {
                            attrs: vec![],
                            by_ref: None,
                            mutability: None,
                            ident: unnamed_fields_variant_pattern_constructor_binding_name(ith),
                            subpat: None,
                        })
                    })
                    .collect::<Punctuated<Pat, Token![,]>>();
                ts.extend(quote! { #variant_name(#pattern) });
            }
            Fields::Named(named) => {
                let pattern = named
                    .named
                    .iter()
                    .map(|field| {
                        Pat::Ident(PatIdent {
                            attrs: vec![],
                            by_ref: None,
                            mutability: None,
                            ident: field.ident.clone().expect("Named field has a name. qed"),
                            subpat: None,
                        })
                    })
                    .collect::<Punctuated<Pat, Token![,]>>();
                ts.extend(quote! { #variant_name{ #pattern } });
            }
        };
    }
}

/// Constructs an enum variant.
#[derive(Hash, Debug)]
struct VariantConstructor(Variant);

impl ToTokens for VariantConstructor {
    fn to_tokens(&self, ts: &mut TokenStream) {
        let variant_name = &self.0.ident;
        let variant_fields = &self.0.fields;
        ts.extend(match variant_fields {
            Fields::Unit => quote! { #variant_name },
            Fields::Unnamed(unnamed) => {
                let constructor = unnamed
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(ith, _field)| {
                        unnamed_fields_variant_pattern_constructor_binding_name(ith)
                    })
                    .collect::<Punctuated<Ident, Token![,]>>();
                quote! { #variant_name (#constructor) }
            }
            Fields::Named(named) => {
                let constructor = named
                    .named
                    .iter()
                    .map(|field| {
                        field
                            .ident
                            .clone()
                            .expect("Named must have named fields. qed")
                    })
                    .collect::<Punctuated<Ident, Token![,]>>();
                quote! { #variant_name { #constructor } }
            }
        });
    }
}

/// Generate the Jfyi and Fatal sub enums.
///
/// `fatal_variants` and `jfyi_variants` cover _all_ variants, if they are forward, they are part of both slices.
/// `forward_variants` enlists all variants that
fn trait_split_impl(
    attr: Attr,
    original: ItemEnum,
    resolution_lut: &IndexMap<Variant, ResolutionMode>,
    jfyi_variants: &[Variant],
    fatal_variants: &[Variant],
) -> Result<TokenStream, syn::Error> {
    if let Attr::Empty = attr {
        return Ok(TokenStream::new());
    }

    let span = original.span();

    let thiserror: Path = parse_quote!(thiserror::Error);
    let thiserror = abs_helper_path(thiserror, span);

    let split_trait = abs_helper_path(Ident::new("Split", span), span);

    let original_ident = original.ident.clone();

    // Generate the splitable types:
    //   Fatal
    let fatal_ident = Ident::new(format!("Fatal{}", original_ident).as_str(), span);
    let mut fatal = original.clone();
    fatal.variants = fatal_variants.iter().cloned().collect();
    fatal.ident = fatal_ident.clone();

    //  Informational (just for your information)
    let jfyi_ident = Ident::new(format!("Jfyi{}", original_ident).as_str(), span);
    let mut jfyi = original.clone();
    jfyi.variants = jfyi_variants.iter().cloned().collect();
    jfyi.ident = jfyi_ident.clone();

    let fatal_patterns = fatal_variants
        .iter()
        .map(|variant| VariantPattern(variant.clone()))
        .collect::<Vec<_>>();
    let jfyi_patterns = jfyi_variants
        .iter()
        .map(|variant| VariantPattern(variant.clone()))
        .collect::<Vec<_>>();

    let fatal_constructors = fatal_variants
        .iter()
        .map(|variant| VariantConstructor(variant.clone()))
        .collect::<Vec<_>>();
    let jfyi_constructors = jfyi_variants
        .iter()
        .map(|variant| VariantConstructor(variant.clone()))
        .collect::<Vec<_>>();

    let mut ts = TokenStream::new();

    ts.extend(quote! {
        impl ::std::convert::From< #fatal_ident> for #original_ident {
            fn from(fatal: #fatal_ident) -> Self {
                match fatal {
                    // Fatal
                    #( #fatal_ident :: #fatal_patterns => Self:: #fatal_constructors, )*
                }
            }
        }

        impl ::std::convert::From< #jfyi_ident> for #original_ident {
            fn from(jfyi: #jfyi_ident) -> Self {
                match jfyi {
                    // JFYI
                    #( #jfyi_ident :: #jfyi_patterns => Self:: #jfyi_constructors, )*
                }
            }
        }

        #[derive(#thiserror, Debug)]
        #fatal

        #[derive(#thiserror, Debug)]
        #jfyi
    });

    // Handle `forward` annotations.
    let trait_fatality = abs_helper_path(format_ident!("Fatality"), Span::call_site());

    // add a a `fatal` variant
    let fatal_patterns_w_if_maybe = fatal_variants
        .iter()
        .map(|variant| {
            let pat = VariantPattern(variant.clone());
            if let Some(ResolutionMode::Forward(_fwd_kw, ident)) = resolution_lut.get(variant) {
                let ident = ident
                    .as_ref()
                    .expect("Forward mode must have an ident at this point. qed");
                quote! { #pat if < _ as #trait_fatality >::is_fatal( & #ident ) }
            } else {
                pat.into_token_stream()
            }
        })
        .collect::<Vec<_>>();

    let jfyi_patterns_w_if_maybe = jfyi_variants
        .iter()
        .map(|variant| {
            let pat = VariantPattern(variant.clone());
            assert!(
                !matches!(resolution_lut.get(variant), None),
                "Cannot be annotated as fatal when in the JFYI slice. qed"
            );
            pat.into_token_stream()
        })
        .collect::<Vec<_>>();

    let split_trait_impl = quote! {

        impl #split_trait for #original_ident {
            type Fatal = #fatal_ident;
            type Jfyi = #jfyi_ident;

            fn split(self) -> ::std::result::Result<Self::Jfyi, Self::Fatal> {
                match self {
                    // Fatal
                    #( Self :: #fatal_patterns_w_if_maybe => Err(#fatal_ident :: #fatal_constructors), )*
                    // JFYI
                    #( Self :: #jfyi_patterns_w_if_maybe => Ok(#jfyi_ident :: #jfyi_constructors), )*
                    // issue: https://github.com/rust-lang/rust/issues/93611#issuecomment-1028844586
                    // #( Self :: #forward_patterns => unreachable!("`Fatality::is_fatal` can only be `true` or `false`, which are covered. qed"), )*
                }
            }
        }
    };
    ts.extend(split_trait_impl);

    Ok(ts)
}

pub(crate) fn fatality_struct_gen(
    attr: Attr,
    mut item: syn::ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let name = item.ident.clone();
    let mut resolution_mode = ResolutionMode::NoAnnotation;

    // remove the `#[fatal]` attribute
    while let Some(idx) = item.attrs.iter().enumerate().find_map(|(idx, attr)| {
        if dbg!(attr.path()).is_ident("fatal") {
            Some(idx)
        } else {
            None
        }
    }) {
        let attr = dbg!(item.attrs.remove(idx));
        if let Ok(_) = dbg!(attr.meta.require_path_only()) {
            // no argument to `#[fatal]` means it's fatal
            resolution_mode = ResolutionMode::Fatal;
        } else {
            // parse whatever was passed to `#[fatal(..)]`.
            resolution_mode = attr.parse_args::<ResolutionMode>()?;
        }
    }

    let (_pat, resolution_mode) = struct_to_pattern(&item, resolution_mode)?;

    // Path to `thiserror`.
    let thiserror: Path = parse_quote!(thiserror::Error);
    let thiserror = abs_helper_path(thiserror, name.span());

    let original_struct = quote! {
        #[derive( #thiserror, Debug)]
        #item
    };

    let mut ts = TokenStream::new();
    ts.extend(original_struct);
    ts.extend(trait_fatality_impl_for_struct(
        &item.ident,
        &resolution_mode,
    ));

    if let Attr::Splitable(kw) = attr {
        return Err(syn::Error::new(
            kw.span(),
            "Cannot use `splitable` on a `struct`",
        ));
    }

    Ok(ts)
}

pub(crate) fn fatality_enum_gen(attr: Attr, item: ItemEnum) -> syn::Result<TokenStream> {
    let name = item.ident.clone();
    let mut original = item.clone();

    let mut resolution_lut = IndexMap::new();
    let mut pattern_lut = IndexMap::new();

    let mut jfyi_variants = Vec::new();
    let mut fatal_variants = Vec::new();

    // if there is not a single fatal annotation, we can just replace `#[fatality]` with `#[derive(::fatality::thiserror::Error, Debug)]`
    // without the intermediate type. But impl `trait Fatality` on-top.
    for variant in original.variants.iter_mut() {
        let mut resolution_mode = ResolutionMode::NoAnnotation;

        // remove the `#[fatal]` attribute
        while let Some(idx) = variant.attrs.iter().enumerate().find_map(|(idx, attr)| {
            if attr.path().is_ident("fatal") {
                Some(idx)
            } else {
                None
            }
        }) {
            let attr = variant.attrs.remove(idx);
            if let Ok(_) = attr.meta.require_path_only() {
                resolution_mode = ResolutionMode::Fatal;
            } else {
                resolution_mode = attr.parse_args::<ResolutionMode>()?;
            }
        }

        // Obtain the patterns for each variant, and the resolution, which can either
        // be `forward`, `true`, or `false`
        // as used in the `trait Fatality`.
        let (pattern, resolution_mode) = enum_variant_to_pattern(variant, resolution_mode)?;
        match resolution_mode {
            ResolutionMode::Forward(_, None) => unreachable!("Must have an ident. qed"),
            ResolutionMode::Forward(_, ref _ident) => {
                jfyi_variants.push(variant.clone());
                fatal_variants.push(variant.clone());
            }
            ResolutionMode::WithExplicitBool(ref b) if b.value() => {
                fatal_variants.push(variant.clone())
            }
            ResolutionMode::WithExplicitBool(_) => jfyi_variants.push(variant.clone()),
            ResolutionMode::Fatal => fatal_variants.push(variant.clone()),
            ResolutionMode::NoAnnotation => jfyi_variants.push(variant.clone()),
        }
        resolution_lut.insert(variant.clone(), resolution_mode);
        pattern_lut.insert(variant.clone(), pattern);
    }

    // Path to `thiserror`.
    let thiserror: Path = parse_quote!(thiserror::Error);
    let thiserror = abs_helper_path(thiserror, name.span());

    let original_enum = quote! {
        #[derive( #thiserror, Debug)]
        #original
    };

    let mut ts = TokenStream::new();
    ts.extend(original_enum);
    ts.extend(trait_fatality_impl_for_enum(
        &original.ident,
        &pattern_lut,
        &resolution_lut,
    ));

    if let Attr::Splitable(_kw) = attr {
        ts.extend(trait_split_impl(
            attr,
            original,
            &resolution_lut,
            &jfyi_variants,
            &fatal_variants,
        ));
    }

    Ok(ts)
}

/// The declaration of `#[fatality(splitable)]` or `#[fatality]`
/// outside the `enum AnError`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Attr {
    Splitable(kw::splitable),
    Empty,
}

impl Parse for Attr {
    fn parse(content: ParseStream) -> syn::Result<Self> {
        let lookahead = content.lookahead1();

        if lookahead.peek(kw::splitable) {
            Ok(Self::Splitable(content.parse::<kw::splitable>()?))
        } else if content.is_empty() {
            Ok(Self::Empty)
        } else {
            Err(lookahead.error())
        }
    }
}
