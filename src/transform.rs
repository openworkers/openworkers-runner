use swc_common::{
    GLOBALS, Globals, Mark, SourceMap, comments::SingleThreadedComments, errors::Handler, sync::Lrc,
};

use swc_ecma_ast::{
    AssignExpr, AssignOp, Expr, ExprStmt, Ident, MemberExpr, MemberProp, ModuleItem, Stmt,
};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::{fixer::fixer, hygiene::hygiene, resolver};
use swc_ecma_transforms_typescript::strip;
use swc_ecma_visit::{VisitMut, VisitMutWith};

use crate::store::CodeType;

/// Transforms `export default X` to `globalThis.default = X`
struct ExportDefaultTransform;

impl VisitMut for ExportDefaultTransform {
    fn visit_mut_module_items(&mut self, items: &mut Vec<ModuleItem>) {
        let mut new_items = Vec::with_capacity(items.len());

        for item in items.drain(..) {
            match item {
                // export default { ... } or export default someExpr
                ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDefaultExpr(export)) => {
                    // Transform to: globalThis.default = <expr>;
                    let assign = create_globalthis_default_assign(*export.expr);
                    new_items.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                        span: export.span,
                        expr: Box::new(assign),
                    })));
                }

                // export default function foo() {} or export default class Foo {}
                ModuleItem::ModuleDecl(swc_ecma_ast::ModuleDecl::ExportDefaultDecl(export)) => {
                    let span = export.span;

                    match export.decl {
                        // export default function() {} or export default function foo() {}
                        swc_ecma_ast::DefaultDecl::Fn(fn_expr) => {
                            let func_expr = Expr::Fn(fn_expr);
                            let assign = create_globalthis_default_assign(func_expr);
                            new_items.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                                span,
                                expr: Box::new(assign),
                            })));
                        }

                        // export default class {} or export default class Foo {}
                        swc_ecma_ast::DefaultDecl::Class(class_expr) => {
                            let class_expr = Expr::Class(class_expr);
                            let assign = create_globalthis_default_assign(class_expr);
                            new_items.push(ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                                span,
                                expr: Box::new(assign),
                            })));
                        }

                        _ => {
                            // Keep as-is for other cases
                            new_items.push(ModuleItem::ModuleDecl(
                                swc_ecma_ast::ModuleDecl::ExportDefaultDecl(export),
                            ));
                        }
                    }
                }

                // Keep all other items as-is
                other => new_items.push(other),
            }
        }

        *items = new_items;
    }
}

/// Creates: globalThis.default = <expr>
fn create_globalthis_default_assign(expr: Expr) -> Expr {
    Expr::Assign(AssignExpr {
        span: Default::default(),
        op: AssignOp::Assign,
        left: swc_ecma_ast::AssignTarget::Simple(swc_ecma_ast::SimpleAssignTarget::Member(
            MemberExpr {
                span: Default::default(),
                obj: Box::new(Expr::Ident(Ident::new_no_ctxt(
                    "globalThis".into(),
                    Default::default(),
                ))),
                prop: MemberProp::Ident(swc_ecma_ast::IdentName::new(
                    "default".into(),
                    Default::default(),
                )),
            },
        )),
        right: Box::new(expr),
    })
}

pub(crate) fn parse_worker_code(code: &[u8], code_type: &CodeType) -> Result<String, String> {
    // Convert bytes to string for JS/TS
    let script =
        std::str::from_utf8(code).map_err(|e| format!("Invalid UTF-8 in worker code: {}", e))?;

    let cm: Lrc<SourceMap> = Default::default();
    let handler = Handler::with_emitter_writer(Box::new(std::io::stderr()), Some(cm.clone()));

    let (filename, syntax) = match code_type {
        CodeType::Javascript => ("script.js", Syntax::Es(Default::default())),
        CodeType::Typescript => (
            "script.ts",
            Syntax::Typescript(TsSyntax {
                tsx: false,
                ..Default::default()
            }),
        ),
        CodeType::Wasm | CodeType::Snapshot => {
            return Err("parse_worker_code called with non-JS code type".to_string());
        }
    };

    let fm = cm.new_source_file(
        Lrc::new(swc_common::FileName::Custom(filename.into())),
        script.to_string(),
    );

    let globals = Globals::default();
    GLOBALS.set(&globals, || {
        transform_code(cm, handler, fm, syntax, code_type)
    })
}

fn transform_code(
    cm: Lrc<SourceMap>,
    handler: Handler,
    fm: Lrc<swc_common::SourceFile>,
    syntax: Syntax,
    code_type: &CodeType,
) -> Result<String, String> {
    // Setup comments tracking
    let comments = SingleThreadedComments::default();

    // Create lexer
    let lexer = Lexer::new(
        syntax,
        Default::default(),
        StringInput::from(&*fm),
        Some(&comments),
    );

    // Parse the code
    let mut parser = Parser::new_from(lexer);

    // Collect any parse errors
    for e in parser.take_errors() {
        e.into_diagnostic(&handler).emit();
    }

    // Parse the program
    let program = parser.parse_program().map_err(|e| {
        let error_msg = format!("Parse error: {:?}", e);
        e.into_diagnostic(&handler).emit();
        error_msg
    })?;

    let unresolved_mark = Mark::new();
    let top_level_mark = Mark::new();

    // Apply resolver transformation
    let mut program = program.apply(resolver(unresolved_mark, top_level_mark, true));

    // Apply TypeScript stripping (only for TS)
    if matches!(code_type, CodeType::Typescript) {
        program = program.apply(strip(unresolved_mark, top_level_mark));
    }

    // Transform export default to globalThis.default
    program.visit_mut_with(&mut ExportDefaultTransform);

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
