use std::sync::Arc;

use swc::config::IsModule;
use swc::config::SourceMapsConfig;
use swc::PrintArgs;
use swc_common::SourceFile;
use swc_common::{errors::Handler, source_map::SourceMap, sync::Lrc, Mark, GLOBALS};
use swc_ecma_ast::EsVersion;
use swc_ecma_ast::Program;
use swc_ecma_parser::Syntax;
use swc_ecma_transforms_typescript::strip;

use crate::store::WorkerData;
use crate::store::WorkerLanguage;

pub(crate) fn parse_worker_code(worker: &WorkerData) -> String {
    match worker.language {
        WorkerLanguage::Javascript => worker.script.clone(),
        WorkerLanguage::Typescript => {
            let cm = Lrc::new(SourceMap::new(swc_common::FilePathMapping::empty()));

            let c = swc::Compiler::new(cm.clone());

            let file = swc_common::FileName::Custom("script.ts".into());

            let fm = cm.new_source_file(Arc::new(file), worker.script.clone());

            return GLOBALS.set(&Default::default(), || to_js(&c, fm.clone()));
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
    let unresolved_mark = Mark::new();

    program.apply(strip(unresolved_mark, top_level_mark))
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
