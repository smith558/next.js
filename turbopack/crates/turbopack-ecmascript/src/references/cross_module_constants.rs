use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use swc_core::common::GLOBALS;
use tracing::instrument;
use turbo_rcstr::RcStr;
use turbo_tasks::{TryJoinIterExt, Vc};
use turbopack_core::{
    compile_time_info::CompileTimeInfo,
    reference_type::ReferenceType,
    resolve::{ResolveResultItem, origin::ResolveOrigin, parse::Request, resolve},
    source::Source,
};

use crate::{
    AnalyzeMode, EcmascriptInputTransforms, EcmascriptModuleAssetType,
    analyzer::{
        ConstantValue, JsValue, ModuleValue, ObjectPart, builtin::replace_builtin,
        graph::create_graph, linker::link, well_known::replace_well_known,
    },
    parse::{ParseResult, parse},
    references::early_value_visitor,
};

#[instrument(level = "info", skip_all, name = "determine cross-module constants")]
pub async fn module_value_to_constants_module(
    module_value: &ModuleValue,
    origin: Vc<Box<dyn ResolveOrigin>>,
    compile_time_info: Vc<CompileTimeInfo>,
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

    let constants = get_constants(**source, compile_time_info).await?;

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
pub async fn get_constants(
    source: Vc<Box<dyn Source>>,
    compile_time_info: Vc<CompileTimeInfo>,
) -> Result<Vc<ConstantsModule>> {
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

    let fun_args_values = Mutex::new(FxHashMap::default());
    let var_cache = Mutex::new(FxHashMap::default());

    let compile_time_info_ref = compile_time_info.await?;

    let mut exports: Vec<(RcStr, ConstantValue)> = var_graph
        .exports
        .iter()
        .map(async |(export_name, binding)| {
            let value = var_graph
                .values
                .get(binding)
                .with_context(|| format!("couldn't find constant binding: {export_name}"))?;

            let linked_value = link(
                &var_graph,
                value.value.clone(),
                &early_value_visitor,
                &async |v| {
                    if let Some((name, _)) = v.get_definable_name(Some(&var_graph))
                        && let Some(value) = compile_time_info_ref.defines.get(&name).await?
                    {
                        return Ok(((&*value).try_into()?, true));
                    }

                    let (mut v, mut modified) =
                        replace_well_known(v, compile_time_info, false).await?;
                    modified = replace_builtin(&mut v) || modified;
                    modified = modified || v.make_nested_operations_unknown();
                    Ok((v, modified))
                },
                &fun_args_values,
                &var_cache,
            )
            .await?;

            if let JsValue::Constant(constant) = linked_value.0 {
                Ok((export_name.as_str().into(), constant))
            } else {
                bail!(
                    "{export_name} is not a constant: {}",
                    value.value.explain(2, 0).0
                );
            }
        })
        .try_join()
        .await?;
    exports.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));

    println!("{} constant exports: {:#?}", path.path, exports);

    Ok(Vc::cell(Some(exports)))
}
