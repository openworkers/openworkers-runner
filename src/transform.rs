use std::sync::Arc;

use swc::config::IsModule;
use swc::config::SourceMapsConfig;
use swc::PrintArgs;
use swc_common::SourceFile;
use swc_common::{errors::Handler, source_map::SourceMap, sync::Lrc, Mark, GLOBALS};
use swc_ecma_ast::EsVersion;
use swc_ecma_ast::Program;
use swc_ecma_parser::Syntax;
use swc_ecma_visit::FoldWith;

use crate::store::WorkerData;
use crate::store::WorkerLanguage;
use openworkers_runtime::FastString;

pub(crate) fn parse_worker_code(worker: &WorkerData) -> FastString {
    match worker.language {
        WorkerLanguage::Javascript => {
            return FastString::from(worker.script.clone());
        }
        WorkerLanguage::Typescript => {
            log::debug!(
                "parsing typescript worker code: {} {}",
                worker.id,
                worker.script
            );

            let cm = Lrc::new(SourceMap::new(swc_common::FilePathMapping::empty()));

            let c = swc::Compiler::new(cm.clone());

            let fm = cm.new_source_file(
                swc_common::FileName::Custom("script.ts".into()),
                worker.script.clone(),
            );

            let js_code = GLOBALS.set(&Default::default(), || to_js(&c, fm.clone()));

            log::debug!("parsed typescript worker code: {} {}", worker.id, js_code);

            return FastString::from(js_code);
        }
    }
}

// https://github.com/swc-project/swc/blob/main/crates/swc_ecma_transforms_typescript/examples/ts_to_js.rs
fn parse(c: &swc::Compiler, fm: Arc<SourceFile>) -> Program {
    let handler = Handler::with_emitter_writer(Box::new(std::io::stderr()), Some(c.cm.clone()));

    let comments = c.comments().clone();

    c.parse_js(
        fm,
        &handler,
        EsVersion::EsNext,
        Syntax::Typescript(Default::default()),
        IsModule::Bool(false),
        Some(&comments),
    )
    .unwrap()
}

fn as_es(c: &swc::Compiler, fm: Arc<SourceFile>) -> Program {
    let program = parse(c, fm);
    let top_level_mark = Mark::new();

    program.fold_with(&mut swc_ecma_transforms_typescript::strip(top_level_mark))
}

fn to_js(c: &swc::Compiler, fm: Arc<SourceFile>) -> String {
    let program = as_es(&c, fm);

    let output = c
        .print(
            &program,
            PrintArgs {
                source_map: SourceMapsConfig::Bool(false),
                ..Default::default()
            },
        )
        .unwrap();

    output.code
}
