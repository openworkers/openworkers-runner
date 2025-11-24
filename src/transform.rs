use swc_common::{
    GLOBALS, Globals, Mark, SourceMap, comments::SingleThreadedComments, errors::Handler, sync::Lrc,
};

use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::{fixer::fixer, hygiene::hygiene, resolver};
use swc_ecma_transforms_typescript::strip;

use crate::store::WorkerData;
use crate::store::WorkerLanguage;

pub(crate) fn parse_worker_code(worker: &WorkerData) -> Result<String, String> {
    match worker.language {
        WorkerLanguage::Javascript => Ok(worker.script.clone()),
        WorkerLanguage::Typescript => {
            let cm: Lrc<SourceMap> = Default::default();
            let handler =
                Handler::with_emitter_writer(Box::new(std::io::stderr()), Some(cm.clone()));

            // Create source file
            let fm = cm.new_source_file(
                Lrc::new(swc_common::FileName::Custom("script.ts".into())),
                worker.script.clone(),
            );

            // Parse and transform with GLOBALS context
            let globals = Globals::default();
            GLOBALS.set(&globals, || typescript_to_javascript(cm, handler, fm))
        }
    }
}

fn typescript_to_javascript(
    cm: Lrc<SourceMap>,
    handler: Handler,
    fm: Lrc<swc_common::SourceFile>,
) -> Result<String, String> {
    // Setup comments tracking
    let comments = SingleThreadedComments::default();

    // Create lexer for TypeScript parsing
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: false, // Not handling TSX for now
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        Some(&comments),
    );

    // Parse the TypeScript code
    let mut parser = Parser::new_from(lexer);

    // Collect any parse errors
    for e in parser.take_errors() {
        e.into_diagnostic(&handler).emit();
    }

    // Parse the program - handle error without borrowing after move
    let program = parser.parse_program().map_err(|e| {
        let error_msg = format!("TypeScript parse error: {:?}", e);
        e.into_diagnostic(&handler).emit();
        error_msg
    })?;

    // Apply transformations using the chain pattern from the example
    let unresolved_mark = Mark::new();
    let top_level_mark = Mark::new();

    // Chain all transformations together like in the SWC example
    // Note: In newer SWC versions, we need to use a different pattern for strip

    // Apply resolver transformation
    let mut program = program.apply(resolver(unresolved_mark, top_level_mark, true));

    // Apply TypeScript stripping
    program = program.apply(strip(unresolved_mark, top_level_mark));

    // Apply hygiene
    program = program.apply(hygiene());

    // Apply fixer
    program = program.apply(fixer(Some(&comments)));

    // Generate JavaScript code
    let mut buf = vec![];
    {
        let writer = JsWriter::new(cm.clone(), "\n", &mut buf, None);
        let mut emitter = Emitter {
            cfg: swc_ecma_codegen::Config::default(),
            cm: cm.clone(),
            comments: Some(&comments),
            wr: writer,
        };

        emitter
            .emit_program(&program)
            .map_err(|e| format!("JavaScript code generation error: {:?}", e))?;
    }

    // Convert to string
    String::from_utf8(buf).map_err(|e| format!("UTF-8 conversion error: {:?}", e))
}
