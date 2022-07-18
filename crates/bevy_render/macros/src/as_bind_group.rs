use bevy_macro_utils::{get_lit_bool, get_lit_str, BevyManifest, Symbol};
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::quote;
use syn::{
    parse::ParseStream, Data, DataStruct, Error, Fields, Lit, LitStr, Meta, NestedMeta, Result,
};

const UNIFORM_ATTRIBUTE_NAME: Symbol = Symbol("uniform");
const TEXTURE_ATTRIBUTE_NAME: Symbol = Symbol("texture");
const SAMPLER_ATTRIBUTE_NAME: Symbol = Symbol("sampler");
const BIND_GROUP_DATA_ATTRIBUTE_NAME: Symbol = Symbol("bind_group_data");

#[derive(Copy, Clone, Debug)]
enum BindingType {
    Uniform,
    Texture,
    Sampler,
}

#[derive(Clone)]
enum BindingState<'a> {
    Free,
    Occupied {
        binding_type: BindingType,
        ident: &'a Ident,
    },
    OccupiedConvertedUniform,
    OccupiedMergableUniform {
        uniform_fields: Vec<&'a syn::Field>,
    },
}

fn get_binding_nested_meta(attr: &syn::Attribute) -> Result<(u32, Vec<NestedMeta>)> {
    match attr.parse_meta() {
        // Parse #[foo(0, ...)]
        Ok(Meta::List(meta)) => {
            let mut nested_iter = meta.nested.into_iter();

            let binding_meta = nested_iter
                .next()
                .ok_or_else(|| Error::new_spanned(attr, "expected #[foo(u32, ...)]"))?;

            let lit_int = if let NestedMeta::Lit(Lit::Int(lit_int)) = binding_meta {
                lit_int
            } else {
                return Err(Error::new_spanned(attr, "expected #[foo(u32, ...)]"));
            };

            Ok((lit_int.base10_parse()?, nested_iter.collect()))
        }
        Ok(other) => Err(Error::new_spanned(other, "expected #[foo(...)]")),
        Err(err) => Err(err),
    }
}

pub fn derive_as_bind_group(ast: syn::DeriveInput) -> Result<TokenStream> {
    let manifest = BevyManifest::default();
    let render_path = manifest.get_path("bevy_render");
    let asset_path = manifest.get_path("bevy_asset");

    let mut binding_states: Vec<BindingState> = Vec::new();
    let mut binding_impls = Vec::new();
    let mut bind_group_entries = Vec::new();
    let mut binding_layouts = Vec::new();
    let mut attr_prepared_data_ident = None;

    // Read struct-level attributes
    for attr in &ast.attrs {
        if let Some(attr_ident) = attr.path.get_ident() {
            if attr_ident == BIND_GROUP_DATA_ATTRIBUTE_NAME {
                if let Ok(prepared_data_ident) =
                    attr.parse_args_with(|input: ParseStream| input.parse::<Ident>())
                {
                    attr_prepared_data_ident = Some(prepared_data_ident);
                }
            } else if attr_ident == UNIFORM_ATTRIBUTE_NAME {
                let (binding_index, converted_shader_type) = attr
                    .parse_args_with(|input: ParseStream| {
                        let binding_index = input
                            .parse::<syn::LitInt>()
                            .and_then(|i| i.base10_parse::<u32>())?;
                        input.parse::<syn::token::Comma>()?;
                        let converted_shader_type = input.parse::<Ident>()?;
                        Ok((binding_index, converted_shader_type))
                    })
                    .map_err(|_| {
                        Error::new_spanned(
                            attr,
                            "struct-level uniform bindings must be in the format: uniform(BINDING_INDEX, ConvertedShaderType)"
                        )
                    })?;

                binding_impls.push(quote! {{
                    use #render_path::render_resource::AsBindGroupShaderType;
                    let mut buffer = #render_path::render_resource::encase::UniformBuffer::new(Vec::new());
                    let converted: #converted_shader_type = self.as_bind_group_shader_type(images);
                    buffer.write(&converted).unwrap();
                    #render_path::render_resource::OwnedBindingResource::Buffer(render_device.create_buffer_with_data(
                        &#render_path::render_resource::BufferInitDescriptor {
                            label: None,
                            usage: #render_path::render_resource::BufferUsages::COPY_DST | #render_path::render_resource::BufferUsages::UNIFORM,
                            contents: buffer.as_ref(),
                        },
                    ))
                }});

                binding_layouts.push(quote!{
                    #render_path::render_resource::BindGroupLayoutEntry {
                        binding: #binding_index,
                        visibility: #render_path::render_resource::ShaderStages::all(),
                        ty: #render_path::render_resource::BindingType::Buffer {
                            ty: #render_path::render_resource::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: Some(<#converted_shader_type as #render_path::render_resource::ShaderType>::min_size()),
                        },
                        count: None,
                    }
                });

                let binding_vec_index = bind_group_entries.len();
                bind_group_entries.push(quote! {
                    #render_path::render_resource::BindGroupEntry {
                        binding: #binding_index,
                        resource: bindings[#binding_vec_index].get_binding(),
                    }
                });

                let required_len = binding_index as usize + 1;
                if required_len > binding_states.len() {
                    binding_states.resize(required_len, BindingState::Free);
                }
                binding_states[binding_index as usize] = BindingState::OccupiedConvertedUniform;
            }
        }
    }

    let fields = match &ast.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(fields),
            ..
        }) => &fields.named,
        _ => {
            return Err(Error::new_spanned(
                ast,
                "Expected a struct with named fields",
            ));
        }
    };

    // Read field-level attributes
    for field in fields.iter() {
        for attr in &field.attrs {
            let attr_ident = if let Some(ident) = attr.path.get_ident() {
                ident
            } else {
                return Err(Error::new_spanned(attr, "Expected identifier"));
            };

            let binding_type = if attr_ident == UNIFORM_ATTRIBUTE_NAME {
                BindingType::Uniform
            } else if attr_ident == TEXTURE_ATTRIBUTE_NAME {
                BindingType::Texture
            } else if attr_ident == SAMPLER_ATTRIBUTE_NAME {
                BindingType::Sampler
            } else {
                continue;
            };

            let (binding_index, nested_meta_items) = get_binding_nested_meta(attr)?;

            let field_name = field.ident.as_ref().unwrap();
            let required_len = binding_index as usize + 1;
            if required_len > binding_states.len() {
                binding_states.resize(required_len, BindingState::Free);
            }

            match &mut binding_states[binding_index as usize] {
                value @ BindingState::Free => {
                    *value = match binding_type {
                        BindingType::Uniform => BindingState::OccupiedMergableUniform {
                            uniform_fields: vec![field],
                        },
                        _ => {
                            // only populate bind group entries for non-uniforms
                            // uniform entries are deferred until the end
                            let binding_vec_index = bind_group_entries.len();
                            bind_group_entries.push(quote! {
                                #render_path::render_resource::BindGroupEntry {
                                    binding: #binding_index,
                                    resource: bindings[#binding_vec_index].get_binding(),
                                }
                            });
                            BindingState::Occupied {
                                binding_type,
                                ident: field_name,
                            }
                        }
                    }
                }
                BindingState::Occupied {
                    binding_type,
                    ident: occupied_ident,
                } => {
                    return Err(Error::new_spanned(
                        attr,
                        format!("The '{field_name}' field cannot be assigned to binding {binding_index} because it is already occupied by the field '{occupied_ident}' of type {binding_type:?}.")
                    ));
                }
                BindingState::OccupiedConvertedUniform => {
                    return Err(Error::new_spanned(
                        attr,
                        format!("The '{field_name}' field cannot be assigned to binding {binding_index} because it is already occupied by a struct-level uniform binding at the same index.")
                    ));
                }
                BindingState::OccupiedMergableUniform { uniform_fields } => match binding_type {
                    BindingType::Uniform => {
                        uniform_fields.push(field);
                    }
                    _ => {
                        return Err(Error::new_spanned(
                                attr,
                                format!("The '{field_name}' field cannot be assigned to binding {binding_index} because it is already occupied by a {:?}.", BindingType::Uniform)
                            ));
                    }
                },
            }

            match binding_type {
                BindingType::Uniform => { /* uniform codegen is deferred to account for combined uniform bindings */
                }
                BindingType::Texture => {
                    let texture_attrs = get_texture_attrs(nested_meta_items)?;

                    let sample_type = match texture_attrs.sample_type {
                        BindingTextureSampleType::Float { filterable } => {
                            quote! { Float { filterable: #filterable } }
                        }
                        BindingTextureSampleType::Depth => quote! { Depth },
                        BindingTextureSampleType::Sint => quote! { Sint },
                        BindingTextureSampleType::Uint => quote! { Uint },
                    };

                    let dimension = match texture_attrs.dimension {
                        BindingTextureDimension::D1 => quote! { D1 },
                        BindingTextureDimension::D2 => quote! { D2 },
                        BindingTextureDimension::D2Array => quote! { D2Array },
                        BindingTextureDimension::Cube => quote! { Cube },
                        BindingTextureDimension::CubeArray => quote! { CubeArray },
                        BindingTextureDimension::D3 => quote! { D3 },
                    };

                    let multisampled = texture_attrs.multisampled;

                    binding_impls.push(quote! {
                        #render_path::render_resource::OwnedBindingResource::TextureView({
                            let handle: Option<&#asset_path::Handle<#render_path::texture::Image>> = (&self.#field_name).into();
                            if let Some(handle) = handle {
                                images.get(handle).ok_or_else(|| #render_path::render_resource::AsBindGroupError::RetryNextUpdate)?.texture_view.clone()
                            } else {
                                fallback_image.texture_view.clone()
                            }
                        })
                    });

                    binding_layouts.push(quote!{
                        #render_path::render_resource::BindGroupLayoutEntry {
                            binding: #binding_index,
                            visibility: #render_path::render_resource::ShaderStages::all(),
                            ty: #render_path::render_resource::BindingType::Texture {
                                multisampled: #multisampled,
                                sample_type: #render_path::render_resource::TextureSampleType::#sample_type,
                                view_dimension: #render_path::render_resource::TextureViewDimension::#dimension,
                            },
                            count: None,
                        }
                    });
                }
                BindingType::Sampler => {
                    let sampler_attrs = get_sampler_attrs(nested_meta_items)?;

                    let sampler_binding_type = match sampler_attrs.sampler_binding_type {
                        SamplerBindingType::Filtering => quote! { Filtering },
                        SamplerBindingType::NonFiltering => quote! { NonFiltering },
                        SamplerBindingType::Comparison => quote! { Comparison },
                    };

                    binding_impls.push(quote! {
                        #render_path::render_resource::OwnedBindingResource::Sampler({
                            let handle: Option<&#asset_path::Handle<#render_path::texture::Image>> = (&self.#field_name).into();
                            if let Some(handle) = handle {
                                images.get(handle).ok_or_else(|| #render_path::render_resource::AsBindGroupError::RetryNextUpdate)?.sampler.clone()
                            } else {
                                fallback_image.sampler.clone()
                            }
                        })
                    });

                    binding_layouts.push(quote!{
                        #render_path::render_resource::BindGroupLayoutEntry {
                            binding: #binding_index,
                            visibility: #render_path::render_resource::ShaderStages::all(),
                            ty: #render_path::render_resource::BindingType::Sampler(#render_path::render_resource::SamplerBindingType::#sampler_binding_type),
                            count: None,
                        }
                    });
                }
            }
        }
    }

    // Produce impls for fields with uniform bindings
    let struct_name = &ast.ident;
    let mut field_struct_impls = Vec::new();
    for (binding_index, binding_state) in binding_states.iter().enumerate() {
        let binding_index = binding_index as u32;
        if let BindingState::OccupiedMergableUniform { uniform_fields } = binding_state {
            let binding_vec_index = bind_group_entries.len();
            bind_group_entries.push(quote! {
                #render_path::render_resource::BindGroupEntry {
                    binding: #binding_index,
                    resource: bindings[#binding_vec_index].get_binding(),
                }
            });
            // single field uniform bindings for a given index can use a straightforward binding
            if uniform_fields.len() == 1 {
                let field = &uniform_fields[0];
                let field_name = field.ident.as_ref().unwrap();
                let field_ty = &field.ty;
                binding_impls.push(quote! {{
                    let mut buffer = #render_path::render_resource::encase::UniformBuffer::new(Vec::new());
                    buffer.write(&self.#field_name).unwrap();
                    #render_path::render_resource::OwnedBindingResource::Buffer(render_device.create_buffer_with_data(
                        &#render_path::render_resource::BufferInitDescriptor {
                            label: None,
                            usage: #render_path::render_resource::BufferUsages::COPY_DST | #render_path::render_resource::BufferUsages::UNIFORM,
                            contents: buffer.as_ref(),
                        },
                    ))
                }});

                binding_layouts.push(quote!{
                    #render_path::render_resource::BindGroupLayoutEntry {
                        binding: #binding_index,
                        visibility: #render_path::render_resource::ShaderStages::all(),
                        ty: #render_path::render_resource::BindingType::Buffer {
                            ty: #render_path::render_resource::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: Some(<#field_ty as #render_path::render_resource::ShaderType>::min_size()),
                        },
                        count: None,
                    }
                });
            // multi-field uniform bindings for a given index require an intermediate struct to derive ShaderType
            } else {
                let uniform_struct_name = Ident::new(
                    &format!("_{struct_name}AsBindGroupUniformStructBindGroup{binding_index}"),
                    Span::call_site(),
                );

                let field_name = uniform_fields.iter().map(|f| f.ident.as_ref().unwrap());
                let field_type = uniform_fields.iter().map(|f| &f.ty);
                field_struct_impls.push(quote! {
                    #[derive(#render_path::render_resource::ShaderType)]
                    struct #uniform_struct_name<'a> {
                        #(#field_name: &'a #field_type,)*
                    }
                });

                let field_name = uniform_fields.iter().map(|f| f.ident.as_ref().unwrap());
                binding_impls.push(quote! {{
                    let mut buffer = #render_path::render_resource::encase::UniformBuffer::new(Vec::new());
                    buffer.write(&#uniform_struct_name {
                        #(#field_name: &self.#field_name,)*
                    }).unwrap();
                    #render_path::render_resource::OwnedBindingResource::Buffer(render_device.create_buffer_with_data(
                        &#render_path::render_resource::BufferInitDescriptor {
                            label: None,
                            usage: #render_path::render_resource::BufferUsages::COPY_DST | #render_path::render_resource::BufferUsages::UNIFORM,
                            contents: buffer.as_ref(),
                        },
                    ))
                }});

                binding_layouts.push(quote!{
                    #render_path::render_resource::BindGroupLayoutEntry {
                        binding: #binding_index,
                        visibility: #render_path::render_resource::ShaderStages::all(),
                        ty: #render_path::render_resource::BindingType::Buffer {
                            ty: #render_path::render_resource::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: Some(<#uniform_struct_name as #render_path::render_resource::ShaderType>::min_size()),
                        },
                        count: None,
                    }
                });
            }
        }
    }

    let generics = ast.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let (prepared_data, get_prepared_data) = if let Some(prepared) = attr_prepared_data_ident {
        let get_prepared_data = quote! { self.into() };
        (quote! {#prepared}, get_prepared_data)
    } else {
        let prepared_data = quote! { () };
        (prepared_data.clone(), prepared_data)
    };

    Ok(TokenStream::from(quote! {
        #(#field_struct_impls)*

        impl #impl_generics #render_path::render_resource::AsBindGroup for #struct_name #ty_generics #where_clause {
            type Data = #prepared_data;
            fn as_bind_group(
                &self,
                layout: &#render_path::render_resource::BindGroupLayout,
                render_device: &#render_path::renderer::RenderDevice,
                images: &#render_path::render_asset::RenderAssets<#render_path::texture::Image>,
                fallback_image: &#render_path::texture::FallbackImage,
            ) -> Result<#render_path::render_resource::PreparedBindGroup<Self>, #render_path::render_resource::AsBindGroupError> {
                let bindings = vec![#(#binding_impls,)*];

                let bind_group = {
                    let descriptor = #render_path::render_resource::BindGroupDescriptor {
                        entries: &[#(#bind_group_entries,)*],
                        label: None,
                        layout: &layout,
                    };
                    render_device.create_bind_group(&descriptor)
                };

                Ok(#render_path::render_resource::PreparedBindGroup {
                    bindings,
                    bind_group,
                    data: #get_prepared_data,
                })
            }

            fn bind_group_layout(render_device: &#render_path::renderer::RenderDevice) -> #render_path::render_resource::BindGroupLayout {
                render_device.create_bind_group_layout(&#render_path::render_resource::BindGroupLayoutDescriptor {
                    entries: &[#(#binding_layouts,)*],
                    label: None,
                })
            }
        }
    }))
}

#[derive(Default)]
enum BindingTextureDimension {
    D1,
    #[default]
    D2,
    D2Array,
    Cube,
    CubeArray,
    D3,
}

enum BindingTextureSampleType {
    Float { filterable: bool },
    Depth,
    Sint,
    Uint,
}

struct TextureAttrs {
    dimension: BindingTextureDimension,
    sample_type: BindingTextureSampleType,
    multisampled: bool,
}

impl Default for BindingTextureSampleType {
    fn default() -> Self {
        BindingTextureSampleType::Float { filterable: true }
    }
}

impl Default for TextureAttrs {
    fn default() -> Self {
        Self {
            dimension: Default::default(),
            sample_type: Default::default(),
            multisampled: true,
        }
    }
}

const DIMENSION: Symbol = Symbol("dimension");
const SAMPLE_TYPE: Symbol = Symbol("sample_type");
const FILTERABLE: Symbol = Symbol("filterable");
const MULTISAMPLED: Symbol = Symbol("multisampled");

// Values for `dimension` attribute.
const DIM_1D: &str = "1d";
const DIM_2D: &str = "2d";
const DIM_3D: &str = "3d";
const DIM_2D_ARRAY: &str = "2d_array";
const DIM_CUBE: &str = "cube";
const DIM_CUBE_ARRAY: &str = "cube_array";

// Values for sample `type` attribute.
const FLOAT: &str = "float";
const DEPTH: &str = "depth";
const S_INT: &str = "s_int";
const U_INT: &str = "u_int";

fn get_texture_attrs(metas: Vec<NestedMeta>) -> Result<TextureAttrs> {
    let mut dimension = Default::default();
    let mut sample_type = Default::default();
    let mut multisampled = Default::default();
    let mut filterable = None;
    let mut filterable_ident = None;

    for meta in metas {
        use syn::{Meta::NameValue, NestedMeta::Meta};
        match meta {
            Meta(NameValue(m)) if m.path == DIMENSION => {
                let value = get_lit_str(DIMENSION, &m.lit)?;
                dimension = get_texture_dimension_value(&value)?;
            }
            Meta(NameValue(m)) if m.path == SAMPLE_TYPE => {
                let value = get_lit_str(SAMPLE_TYPE, &m.lit)?;
                sample_type = get_texture_sample_type_value(&value)?;
            }
            Meta(NameValue(m)) if m.path == MULTISAMPLED => {
                multisampled = get_lit_bool(MULTISAMPLED, &m.lit)?;
            }
            Meta(NameValue(m)) if m.path == FILTERABLE => {
                filterable = get_lit_bool(FILTERABLE, &m.lit)?.into();
                filterable_ident = m.path.into();
            }
            Meta(NameValue(m)) => {
                return Err(Error::new_spanned(
                    m.path,
                    "Not a valid name. Available attributes: `dimension`, `sample_type`, `multisampled`, or `filterable`."
                ));
            }
            _ => {
                return Err(Error::new_spanned(
                    meta,
                    "Not a name value pair: `foo = \"...\"`",
                ));
            }
        }
    }

    // Resolve `filterable` since the float
    // sample type is the one that contains the value.
    if let Some(filterable) = filterable {
        let path = filterable_ident.unwrap();
        match sample_type {
            BindingTextureSampleType::Float { filterable: _ } => {
                sample_type = BindingTextureSampleType::Float { filterable }
            }
            _ => {
                return Err(Error::new_spanned(
                    path.clone(),
                    "Type must be `float` to use the `filterable` attribute.",
                ));
            }
        };
    }

    Ok(TextureAttrs {
        dimension,
        sample_type,
        multisampled,
    })
}

fn get_texture_dimension_value(lit_str: &LitStr) -> Result<BindingTextureDimension> {
    match lit_str.value().as_str() {
        DIM_1D => Ok(BindingTextureDimension::D1),
        DIM_2D => Ok(BindingTextureDimension::D2),
        DIM_2D_ARRAY => Ok(BindingTextureDimension::D2Array),
        DIM_3D => Ok(BindingTextureDimension::D3),
        DIM_CUBE => Ok(BindingTextureDimension::Cube),
        DIM_CUBE_ARRAY => Ok(BindingTextureDimension::CubeArray),

        _ => Err(Error::new_spanned(
            lit_str,
            "Not a valid dimension. Must be `1d`, `2d`, `2d_array`, `3d`, `cube` or `cube_array`.",
        )),
    }
}

fn get_texture_sample_type_value(lit_str: &LitStr) -> Result<BindingTextureSampleType> {
    match lit_str.value().as_str() {
        FLOAT => Ok(BindingTextureSampleType::Float { filterable: true }),
        DEPTH => Ok(BindingTextureSampleType::Depth),
        S_INT => Ok(BindingTextureSampleType::Sint),
        U_INT => Ok(BindingTextureSampleType::Uint),

        _ => Err(Error::new_spanned(
            lit_str,
            "Not a valid sample type. Must be `float`, `depth`, `s_int` or `u_int`.",
        )),
    }
}

#[derive(Default)]
struct SamplerAttrs {
    sampler_binding_type: SamplerBindingType,
}

#[derive(Default)]
enum SamplerBindingType {
    #[default]
    Filtering,
    NonFiltering,
    Comparison,
}

const SAMPLER_TYPE: Symbol = Symbol("sampler_type");

const FILTERING: &str = "filtering";
const NON_FILTERING: &str = "non_filtering";
const COMPARISON: &str = "comparison";

fn get_sampler_attrs(metas: Vec<NestedMeta>) -> Result<SamplerAttrs> {
    let mut sampler_binding_type = Default::default();

    for meta in metas {
        use syn::{Meta::NameValue, NestedMeta::Meta};
        match meta {
            Meta(NameValue(m)) if m.path == SAMPLER_TYPE => {
                let value = get_lit_str(DIMENSION, &m.lit)?;
                sampler_binding_type = get_sampler_binding_type_value(&value)?;
            }
            Meta(NameValue(m)) => {
                return Err(Error::new_spanned(
                    m.path,
                    "Not a valid name. Available attributes: `sampler_type`.",
                ));
            }
            _ => {
                return Err(Error::new_spanned(
                    meta,
                    "Not a name value pair: `foo = \"...\"`",
                ));
            }
        }
    }

    Ok(SamplerAttrs {
        sampler_binding_type,
    })
}

fn get_sampler_binding_type_value(lit_str: &LitStr) -> Result<SamplerBindingType> {
    match lit_str.value().as_str() {
        FILTERING => Ok(SamplerBindingType::Filtering),
        NON_FILTERING => Ok(SamplerBindingType::NonFiltering),
        COMPARISON => Ok(SamplerBindingType::Comparison),

        _ => Err(Error::new_spanned(
            lit_str,
            "Not a valid dimension. Must be `filtering`, `non_filtering`, or `comparison`.",
        )),
    }
}
