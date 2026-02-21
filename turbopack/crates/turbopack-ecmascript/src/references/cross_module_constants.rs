use anyhow::{Context, Result, bail};
use swc_core::common::GLOBALS;
use tracing::instrument;
use turbo_rcstr::RcStr;
use turbo_tasks::Vc;
use turbopack_core::{
    reference_type::ReferenceType,
    resolve::{ResolveResultItem, origin::ResolveOrigin, parse::Request, resolve},
    source::Source,
};

use crate::{
    AnalyzeMode, EcmascriptInputTransforms, EcmascriptModuleAssetType,
    analyzer::{ConstantValue, JsValue, ModuleValue, ObjectPart, graph::create_graph},
    parse::{ParseResult, parse},
};

#[instrument(level = "info", skip_all, name = "determine cross-module constants")]
pub async fn module_value_to_constants_module(
    module_value: &ModuleValue,
    origin: Vc<Box<dyn ResolveOrigin>>,
) -> Result<Option<JsValue>> {
    let request = module_value.module.to_string_lossy();
    if !request.contains(".constants") {
        return Ok(None);
    }

    let source = resolve(
        origin.origin_path().await?.parent(),
        // TODO a special reference type plus module type to plug this into the module rule system?
        // And then `Vc::try_downcast<ConstantsProvider>(module).get_constants()`
        ReferenceType::Undefined,
        Request::parse_string(request.into()),
        origin.resolve_options(),
    )
    .await?;

    let Some(ResolveResultItem::Source(source)) = source.primary.first().as_ref().map(|v| &v.1)
    else {
        bail!("not a source, {:?}", source.primary);
    };

    let constants = get_constants(**source).await?;

    if let Some(constants) = &*constants {
        Ok(Some(JsValue::frozen_object(
            constants
                .iter()
                .map(|(key, value)| {
                    ObjectPart::KeyValue(
                        JsValue::Constant(ConstantValue::Str(key.clone().into())),
                        JsValue::Constant(value.clone()),
                    )
                })
                .collect(),
        )))
    } else {
        Ok(None)
    }
}

#[turbo_tasks::value(transparent)]
struct ConstantsModule(Option<Vec<(RcStr, ConstantValue)>>);

#[turbo_tasks::function]
pub async fn get_constants(source: Vc<Box<dyn Source>>) -> Result<Vc<ConstantsModule>> {
    let path = source.ident().path().await?;

    let result = &*parse(
        source,
        if path.path.ends_with(".ts") {
            EcmascriptModuleAssetType::Typescript {
                tsx: false,
                analyze_types: false,
            }
        } else if path.path.ends_with(".tsx") {
            EcmascriptModuleAssetType::Typescript {
                tsx: true,
                analyze_types: false,
            }
        } else {
            EcmascriptModuleAssetType::Ecmascript
        },
        EcmascriptInputTransforms::empty(),
        false,
        false,
    )
    .await?;

    let ParseResult::Ok {
        program,
        eval_context,
        globals,
        ..
    } = result
    else {
        // The `parse` call has already emitted parse issues in case of `ParseResult::Unparsable`
        return Ok(Vc::cell(None));
    };

    let var_graph = {
        let _span = tracing::trace_span!("analyze variable values").entered();
        GLOBALS.set(globals, || {
            create_graph(program, eval_context, AnalyzeMode::Tracing)
        })
    };

    let mut exports: Vec<(RcStr, ConstantValue)> = var_graph
        .exports
        .iter()
        .map(|(export_name, binding)| {
            Ok((
                export_name.as_str().into(),
                var_graph
                    .values
                    .get(binding)
                    .and_then(|value| {
                        if let JsValue::Constant(constant) = value.value.clone() {
                            Some(constant)
                        } else {
                            None
                        }
                    })
                    .with_context(|| format!("not a constant: {export_name}"))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    exports.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    println!("{} constant exports: {:#?}", path.path, exports);

    Ok(Vc::cell(Some(exports)))
}
