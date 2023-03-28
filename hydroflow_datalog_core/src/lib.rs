use rust_sitter::{
    errors::{ParseError, ParseErrorReason},
    Spanned,
};
use std::collections::{HashMap, HashSet};

use hydroflow_lang::{
    diagnostic::{Diagnostic, Level},
    graph::{partition_graph, FlatGraphBuilder, HydroflowGraph},
    parse::{IndexInt, Indexing, Pipeline, PipelineLink},
};
use proc_macro2::{Span, TokenStream};
use syn::{parse_quote, parse_quote_spanned};

mod grammar;
mod join_plan;
mod util;

use grammar::datalog::*;
use join_plan::*;
use util::{repeat_tuple, Counter};

static MAGIC_RELATIONS: [&str; 1] = ["less_than"];

pub fn parse_pipeline(
    code_str: &rust_sitter::Spanned<String>,
    get_span: &impl Fn((usize, usize)) -> Span,
) -> Result<Pipeline, Vec<Diagnostic>> {
    syn::LitStr::new(&code_str.value, get_span(code_str.span))
        .parse()
        .map_err(|err| {
            vec![Diagnostic {
                span: err.span(),
                level: Level::Error,
                message: format!("Failed to parse input pipeline: {}", err),
            }]
        })
}

pub fn gen_hydroflow_graph(
    literal: proc_macro2::Literal,
) -> Result<HydroflowGraph, Vec<Diagnostic>> {
    let offset = {
        // This includes the quotes, i.e. 'r#"my test"#' or '"hello\nworld"'.
        let source_str = literal.to_string();
        let mut source_chars = source_str.chars();
        if Some('r') != source_chars.next() {
            return Err(vec![Diagnostic {
                span: literal.span(),
                level: Level::Error,
                message:
                    r##"Input must be a raw string `r#"..."#` for correct diagnostic messages."##
                        .to_owned(),
            }]);
        }
        let hashes = source_chars.take_while(|&c| '#' == c).count();
        2 + hashes
    };

    let get_span = |(start, end): (usize, usize)| {
        let subspan = literal.subspan(start + offset..end + offset);
        subspan.unwrap_or(Span::call_site())
    };

    let str_node: syn::LitStr = parse_quote!(#literal);
    let actual_str = str_node.value();
    let program: Program =
        grammar::datalog::parse(&actual_str).map_err(|e| handle_errors(e, &get_span))?;

    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut asyncs = Vec::new();
    let mut rules = Vec::new();

    for stmt in &program.rules {
        match stmt {
            Declaration::Input(_, ident, hf_code) => {
                assert!(!MAGIC_RELATIONS.contains(&ident.name.as_str()));
                inputs.push((ident, hf_code))
            }
            Declaration::Output(_, ident, hf_code) => {
                assert!(!MAGIC_RELATIONS.contains(&ident.name.as_str()));
                outputs.push((ident, hf_code))
            }
            Declaration::Async(_, ident, send_hf, recv_hf) => {
                assert!(!MAGIC_RELATIONS.contains(&ident.name.as_str()));
                asyncs.push((ident, send_hf, recv_hf))
            }
            Declaration::Rule(rule) => {
                assert!(!MAGIC_RELATIONS.contains(&rule.target.name.name.as_str()));
                rules.push(rule)
            }
        }
    }

    let mut flat_graph_builder = FlatGraphBuilder::new();
    let mut tee_counter = HashMap::new();
    let mut merge_counter = HashMap::new();

    let mut created_rules = HashSet::new();
    for decl in &program.rules {
        let target_ident = match decl {
            Declaration::Input(_, ident, _) => ident.clone(),
            Declaration::Output(_, ident, _) => ident.clone(),
            Declaration::Async(_, ident, _, _) => ident.clone(),
            Declaration::Rule(rule) => rule.target.name.clone(),
        };

        if !created_rules.contains(&target_ident.value) {
            created_rules.insert(target_ident.value.clone());
            let name = syn::Ident::new(&target_ident.name, get_span(target_ident.span));
            flat_graph_builder
                .add_statement(parse_quote_spanned!(get_span(target_ident.span)=> #name = merge() -> unique::<'tick>() -> tee()));
        }
    }

    for (target, hf_code) in inputs {
        let my_merge_index = merge_counter
            .entry(target.name.clone())
            .or_insert_with(|| 0..)
            .next()
            .expect("Out of merge indices");

        let my_merge_index_lit =
            syn::LitInt::new(&format!("{}", my_merge_index), get_span(target.span));
        let name = syn::Ident::new(&target.name, get_span(target.span));

        let input_pipeline: Pipeline = parse_pipeline(&hf_code.code, &get_span)?;

        flat_graph_builder.add_statement(parse_quote_spanned! {get_span(target.span)=>
            #input_pipeline -> [#my_merge_index_lit] #name
        });
    }

    for (target, hf_code) in outputs {
        let my_tee_index = tee_counter
            .entry(target.name.clone())
            .or_insert_with(|| 0..)
            .next()
            .expect("Out of tee indices");

        let my_tee_index_lit =
            syn::LitInt::new(&format!("{}", my_tee_index), get_span(target.span));
        let target_ident = syn::Ident::new(&target.name, get_span(target.span));

        let output_pipeline: Pipeline = parse_pipeline(&hf_code.code, &get_span)?;

        flat_graph_builder.add_statement(parse_quote_spanned! {get_span(target.span)=>
            #target_ident [#my_tee_index_lit] -> #output_pipeline
        });
    }

    for (target, send_hf, recv_hf) in asyncs {
        let async_send_pipeline = format!("{}_async_send", target.name);
        let async_send_pipeline = syn::Ident::new(&async_send_pipeline, get_span(target.span));

        let recv_merge_index = merge_counter
            .entry(target.name.clone())
            .or_insert_with(|| 0..)
            .next()
            .expect("Out of merge indices");

        let recv_merge_index_lit =
            syn::LitInt::new(&format!("{}", recv_merge_index), get_span(target.span));
        let target_ident = syn::Ident::new(&target.name, get_span(target.span));

        let send_pipeline: Pipeline = parse_pipeline(&send_hf.code, &get_span)?;
        let recv_pipeline: Pipeline = parse_pipeline(&recv_hf.code, &get_span)?;

        flat_graph_builder.add_statement(parse_quote_spanned! {get_span(target.span)=>
            #async_send_pipeline = merge() -> unique::<'tick>() -> #send_pipeline
        });

        flat_graph_builder.add_statement(parse_quote_spanned! {get_span(target.span)=>
            #recv_pipeline -> unique::<'tick>() -> [#recv_merge_index_lit] #target_ident
        });
    }

    let mut next_join_idx = 0..;
    let mut diagnostics = Vec::new();
    for rule in rules {
        generate_rule(
            rule,
            &mut flat_graph_builder,
            &mut tee_counter,
            &mut merge_counter,
            &mut next_join_idx,
            &mut diagnostics,
            &get_span,
        );
    }

    if !diagnostics.is_empty() {
        Err(diagnostics)
    } else {
        let (flat_graph, mut diagnostics) = flat_graph_builder.build();
        diagnostics.retain(Diagnostic::is_error);
        if !diagnostics.is_empty() {
            Err(diagnostics)
        } else {
            Ok(flat_graph)
        }
    }
}

fn handle_errors(
    errors: Vec<ParseError>,
    get_span: &impl Fn((usize, usize)) -> Span,
) -> Vec<Diagnostic> {
    let mut diagnostics = vec![];
    for error in errors {
        let reason = error.reason;
        let my_span = get_span((error.start, error.end));
        match reason {
            ParseErrorReason::UnexpectedToken(msg) => {
                diagnostics.push(Diagnostic::spanned(
                    my_span,
                    Level::Error,
                    format!("Unexpected Token: '{msg}'", msg = msg),
                ));
            }
            ParseErrorReason::MissingToken(msg) => {
                diagnostics.push(Diagnostic::spanned(
                    my_span,
                    Level::Error,
                    format!("Missing Token: '{msg}'", msg = msg),
                ));
            }
            ParseErrorReason::FailedNode(parse_errors) => {
                if parse_errors.is_empty() {
                    diagnostics.push(Diagnostic::spanned(
                        my_span,
                        Level::Error,
                        "Failed to parse",
                    ));
                } else {
                    diagnostics.extend(handle_errors(parse_errors, get_span));
                }
            }
        }
    }

    diagnostics
}

pub fn hydroflow_graph_to_program(flat_graph: HydroflowGraph, root: TokenStream) -> TokenStream {
    let partitioned_graph =
        partition_graph(flat_graph).expect("Failed to partition (cycle detected).");

    let mut diagnostics = Vec::new();
    let code_tokens = partitioned_graph.as_code(&root, true, &mut diagnostics);
    assert_eq!(
        0,
        diagnostics.len(),
        "Operator diagnostic occured during codegen"
    );

    code_tokens
}

fn generate_rule(
    rule: &Spanned<Rule>,
    flat_graph_builder: &mut FlatGraphBuilder,
    tee_counter: &mut HashMap<String, Counter>,
    merge_counter: &mut HashMap<String, Counter>,
    next_join_idx: &mut Counter,
    diagnostics: &mut Vec<Diagnostic>,
    get_span: &impl Fn((usize, usize)) -> Span,
) {
    let target = &rule.target.name;
    let target_ident = syn::Ident::new(&target.name, get_span(target.span));

    let sources: Vec<Atom> = rule.sources.to_vec();

    // TODO(shadaj): smarter plans
    let mut plan: JoinPlan = sources
        .iter()
        .filter_map(|x| match x {
            Atom::Relation(negated, e) => {
                if negated.is_none() && !MAGIC_RELATIONS.contains(&e.name.name.as_str()) {
                    Some(JoinPlan::Source(e))
                } else {
                    None
                }
            }
            _ => None,
        })
        .reduce(|a, b| JoinPlan::Join(Box::new(a), Box::new(b)))
        .unwrap();

    plan = sources
        .iter()
        .filter_map(|x| match x {
            Atom::Relation(negated, e) => {
                if negated.is_some() {
                    Some(JoinPlan::Source(e))
                } else {
                    None
                }
            }
            _ => None,
        })
        .fold(plan, |a, b| JoinPlan::AntiJoin(Box::new(a), Box::new(b)));

    let predicates = sources
        .iter()
        .filter_map(|x| match x {
            Atom::Predicate(e) => Some(e),
            _ => None,
        })
        .collect::<Vec<_>>();

    if !predicates.is_empty() {
        plan = JoinPlan::Predicate(predicates, Box::new(plan))
    }

    plan = sources.iter().fold(plan, |acc, atom| match atom {
        Atom::Relation(negated, e) => {
            if MAGIC_RELATIONS.contains(&e.name.name.as_str()) {
                match e.name.name.as_str() {
                    "less_than" => {
                        assert!(negated.is_none());
                        JoinPlan::MagicNatLt(
                            Box::new(acc),
                            e.fields[0].value.clone(),
                            e.fields[1].value.clone(),
                        )
                    }
                    o => panic!("Unknown magic relation {}", o),
                }
            } else {
                acc
            }
        }
        _ => acc,
    });

    let out_expanded = expand_join_plan(
        &plan,
        flat_graph_builder,
        tee_counter,
        next_join_idx,
        rule.span,
        get_span,
    );

    let after_join = apply_aggregations(rule, &out_expanded, diagnostics, get_span);

    let my_merge_index = merge_counter
        .entry(target.name.clone())
        .or_insert_with(|| 0..)
        .next()
        .expect("Out of merge indices");

    let my_merge_index_lit = syn::LitInt::new(&format!("{}", my_merge_index), Span::call_site());

    let after_join_and_send: Pipeline = match rule.rule_type.value {
        RuleType::Sync(_) => {
            if rule.target.at_node.is_some() {
                panic!("Rule must be async to send data to other nodes")
            }

            parse_quote_spanned!(get_span(rule.rule_type.span)=> #after_join -> [#my_merge_index_lit] #target_ident)
        }
        RuleType::NextTick(_) => {
            if rule.target.at_node.is_some() {
                panic!("Rule must be async to send data to other nodes")
            }

            parse_quote_spanned!(get_span(rule.rule_type.span)=> #after_join -> next_tick() -> [#my_merge_index_lit] #target_ident)
        }
        RuleType::Async(_) => {
            if rule.target.at_node.is_none() {
                panic!("Async rules are only for sending data to other nodes")
            }

            let exprs_get_data = rule
                .target
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| -> syn::Expr {
                    let syn_index = syn::Index::from(i);
                    parse_quote_spanned!(get_span(f.span)=> v.#syn_index)
                });

            let syn_target_index = syn::Index::from(rule.target.fields.len());

            let v_type = repeat_tuple::<syn::Type, syn::Type>(
                || parse_quote!(_),
                rule.target.fields.len() + 1,
            );

            let send_pipeline_ident = syn::Ident::new(
                &format!("{}_async_send", &rule.target.name.name),
                get_span(rule.target.name.span),
            );

            parse_quote_spanned!(get_span(rule.rule_type.span)=> #after_join -> map(|v: #v_type| (v.#syn_target_index, (#(#exprs_get_data, )*))) -> #send_pipeline_ident)
        }
    };

    let out_name = out_expanded.name;
    // If the output comes with a tee index, we must read with that. This only happens when we are
    // directly outputting a transformation of a single relation on the RHS.
    let out_indexing = out_expanded.tee_idx.map(|i| Indexing {
        bracket_token: syn::token::Bracket::default(),
        index: hydroflow_lang::parse::PortIndex::Int(IndexInt {
            value: i,
            span: Span::call_site(),
        }),
    });
    flat_graph_builder.add_statement(hydroflow_lang::parse::HfStatement::Pipeline(
        Pipeline::Link(PipelineLink {
            lhs: Box::new(parse_quote!(#out_name #out_indexing)), // out_name[idx]
            arrow: parse_quote!(->),
            rhs: Box::new(after_join_and_send),
        }),
    ));
}

fn gen_value_expr(
    expr: &ValueExpr,
    field_use_count: &HashMap<String, i32>,
    field_use_cur: &mut HashMap<String, i32>,
    out_expanded: &IntermediateJoinNode,
    diagnostics: &mut Vec<Diagnostic>,
    get_span: &dyn Fn((usize, usize)) -> Span,
) -> syn::Expr {
    match expr {
        ValueExpr::Ident(ident) => {
            if let Some(col) = out_expanded.variable_mapping.get(&ident.name) {
                let cur_count = field_use_cur
                    .entry(ident.name.clone())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);

                let source_col_idx = syn::Index::from(*col);
                let base = parse_quote_spanned!(get_span(ident.span)=> row.#source_col_idx);

                if *cur_count < field_use_count[&ident.name] && field_use_count[&ident.name] > 1 {
                    parse_quote!(#base.clone())
                } else {
                    base
                }
            } else {
                diagnostics.push(Diagnostic::spanned(
                    get_span(ident.span),
                    Level::Error,
                    format!("Could not find column {} in RHS of rule", &ident.name),
                ));
                parse_quote!(())
            }
        }
        ValueExpr::Integer(i) => syn::Expr::Lit(syn::ExprLit {
            attrs: Vec::new(),
            lit: syn::Lit::Int(syn::LitInt::new(&i.to_string(), get_span(i.span))),
        }),
        ValueExpr::Add(l, _, r) => {
            let l = gen_value_expr(
                l,
                field_use_count,
                field_use_cur,
                out_expanded,
                diagnostics,
                get_span,
            );
            let r = gen_value_expr(
                r,
                field_use_count,
                field_use_cur,
                out_expanded,
                diagnostics,
                get_span,
            );
            parse_quote!(#l + #r)
        }
        ValueExpr::Sub(l, _, r) => {
            let l = gen_value_expr(
                l,
                field_use_count,
                field_use_cur,
                out_expanded,
                diagnostics,
                get_span,
            );
            let r = gen_value_expr(
                r,
                field_use_count,
                field_use_cur,
                out_expanded,
                diagnostics,
                get_span,
            );
            parse_quote!(#l - #r)
        }
    }
}

fn gen_target_expr(
    expr: &TargetExpr,
    field_use_count: &HashMap<String, i32>,
    field_use_cur: &mut HashMap<String, i32>,
    out_expanded: &IntermediateJoinNode,
    diagnostics: &mut Vec<Diagnostic>,
    get_span: &dyn Fn((usize, usize)) -> Span,
) -> syn::Expr {
    match expr {
        TargetExpr::Expr(expr) => gen_value_expr(
            expr,
            field_use_count,
            field_use_cur,
            out_expanded,
            diagnostics,
            get_span,
        ),
        TargetExpr::Aggregation(Aggregation { ident, .. }) => gen_value_expr(
            &ValueExpr::Ident(ident.clone()),
            field_use_count,
            field_use_cur,
            out_expanded,
            diagnostics,
            get_span,
        ),
    }
}

fn apply_aggregations(
    rule: &Rule,
    out_expanded: &IntermediateJoinNode,
    diagnostics: &mut Vec<Diagnostic>,
    get_span: &impl Fn((usize, usize)) -> Span,
) -> Pipeline {
    let mut aggregations = vec![];
    let mut group_by_exprs = vec![];
    let mut agg_exprs = vec![];

    let mut field_use_count = HashMap::new();
    for field in rule
        .target
        .fields
        .iter()
        .chain(rule.target.at_node.iter().map(|n| &n.node))
    {
        for ident in field.idents() {
            field_use_count
                .entry(ident.name.clone())
                .and_modify(|e| *e += 1)
                .or_insert(1);
        }
    }

    let mut field_use_cur = HashMap::new();
    for field in rule
        .target
        .fields
        .iter()
        .chain(rule.target.at_node.iter().map(|n| &n.node))
    {
        let expr: syn::Expr = gen_target_expr(
            field,
            &field_use_count,
            &mut field_use_cur,
            out_expanded,
            diagnostics,
            get_span,
        );

        match field.value {
            TargetExpr::Expr(_) => {
                group_by_exprs.push(expr);
            }
            TargetExpr::Aggregation(_) => {
                aggregations.push(field);
                agg_exprs.push(expr);
            }
        }
    }

    let flattened_tuple_type = &out_expanded.tuple_type;

    if agg_exprs.is_empty() {
        parse_quote!(map(|row: #flattened_tuple_type| (#(#group_by_exprs, )*)))
    } else {
        let agg_initial =
            repeat_tuple::<syn::Expr, syn::Expr>(|| parse_quote!(None), agg_exprs.len());

        let group_by_input_type =
            repeat_tuple::<syn::Type, syn::Type>(|| parse_quote!(_), group_by_exprs.len());

        let agg_input_type =
            repeat_tuple::<syn::Type, syn::Type>(|| parse_quote!(_), agg_exprs.len());
        let agg_type: syn::Type =
            repeat_tuple::<syn::Type, syn::Type>(|| parse_quote!(Option<_>), agg_exprs.len());

        let group_by_stmts: Vec<syn::Stmt> = aggregations
            .iter()
            .enumerate()
            .map(|(i, agg)| {
                let idx = syn::Index::from(i);
                let old_at_index: syn::Expr = parse_quote!(old.#idx);
                let val_at_index: syn::Expr = parse_quote!(val.#idx);

                let agg_expr: syn::Expr = match &agg.value {
                    TargetExpr::Aggregation(Aggregation { tpe, .. }) => match tpe {
                        AggregationType::Min(_) => {
                            parse_quote!(std::cmp::min(prev, #val_at_index))
                        }
                        AggregationType::Max(_) => {
                            parse_quote!(std::cmp::max(prev, #val_at_index))
                        }
                        AggregationType::Sum(_) => {
                            parse_quote!(prev + #val_at_index)
                        }
                        AggregationType::Count(_) => {
                            parse_quote!(prev + 1)
                        }
                        AggregationType::Choose(_) => {
                            parse_quote!(prev) // choose = select any 1 element from the relation. By default we select the 1st.
                        }
                    },
                    _ => panic!(),
                };

                let agg_initial: syn::Expr = match &agg.value {
                    TargetExpr::Aggregation(Aggregation { tpe, .. }) => match tpe {
                        AggregationType::Min(_)
                        | AggregationType::Max(_)
                        | AggregationType::Sum(_)
                        | AggregationType::Choose(_) => {
                            parse_quote!(#val_at_index)
                        }
                        AggregationType::Count(_) => {
                            parse_quote!(1)
                        }
                    },
                    _ => panic!(),
                };

                parse_quote_spanned! {get_span(agg.span) =>
                    #old_at_index = if let Some(prev) = #old_at_index.take() {
                        Some(#agg_expr)
                    } else {
                        Some(#agg_initial)
                    };
                }
            })
            .collect();

        let group_by_fn: syn::Expr = parse_quote!(|old: &mut #agg_type, val: #agg_input_type| {
            #(#group_by_stmts)*
        });

        let mut after_group_lookups: Vec<syn::Expr> = vec![];
        let mut group_key_idx = 0;
        let mut agg_idx = 0;
        for field in rule
            .target
            .fields
            .iter()
            .chain(rule.target.at_node.iter().map(|n| &n.node))
        {
            match field.value {
                TargetExpr::Expr(_) => {
                    let idx = syn::Index::from(group_key_idx);
                    after_group_lookups.push(parse_quote_spanned!(get_span(field.span)=> g.#idx));
                    group_key_idx += 1;
                }
                TargetExpr::Aggregation(_) => {
                    let idx = syn::Index::from(agg_idx);
                    after_group_lookups
                        .push(parse_quote_spanned!(get_span(field.span)=> a.#idx.unwrap()));
                    agg_idx += 1;
                }
            }
        }

        let pre_group_by_map: syn::Expr = parse_quote!(|row: #flattened_tuple_type| ((#(#group_by_exprs, )*), (#(#agg_exprs, )*)));
        let after_group_map: syn::Expr = parse_quote!(|(g, a)| (#(#after_group_lookups, )*));

        parse_quote! {
            map(#pre_group_by_map) -> group_by::<'tick, #group_by_input_type, #agg_type>(|| #agg_initial, #group_by_fn) -> map(#after_group_map)
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::gen_hydroflow_graph;
    use super::hydroflow_graph_to_program;

    macro_rules! test_snapshots {
        ($program:literal) => {
            let flat_graph = gen_hydroflow_graph(parse_quote!($program)).unwrap();

            let flat_graph_ref = &flat_graph;
            insta::with_settings!({snapshot_suffix => "surface_graph"}, {
                insta::assert_display_snapshot!(flat_graph_ref.surface_syntax_string());
            });

            let tokens = hydroflow_graph_to_program(flat_graph, quote::quote! { hydroflow });
            let out: syn::Stmt = syn::parse_quote!(#tokens);
            let wrapped: syn::File = parse_quote! {
                fn main() {
                    #out
                }
            };

            insta::with_settings!({snapshot_suffix => "datalog_program"}, {
                insta::assert_display_snapshot!(
                    prettyplease::unparse(&wrapped)
                );
            });
        };
    }

    #[test]
    fn minimal_program() {
        test_snapshots!(
            r#"
            .input input `source_stream(input)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(y, x) :- input(x, y).
            "#
        );
    }

    #[test]
    fn join_with_self() {
        test_snapshots!(
            r#"
            .input input `source_stream(input)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, y) :- input(x, y), input(y, x).
            "#
        );
    }

    #[test]
    fn join_with_other() {
        test_snapshots!(
            r#"
            .input in1 `source_stream(in1)`
            .input in2 `source_stream(in2)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, y) :- in1(x, y), in2(y, x).
            "#
        );
    }

    #[test]
    fn multiple_contributors() {
        test_snapshots!(
            r#"
            .input in1 `source_stream(in1)`
            .input in2 `source_stream(in2)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, y) :- in1(x, y).
            out(x, y) :- in2(y, x).
            "#
        );
    }

    #[test]
    fn transitive_closure() {
        test_snapshots!(
            r#"
            .input edges `source_stream(edges)`
            .input seed_reachable `source_stream(seed_reachable)`
            .output reachable `for_each(|v| reachable.send(v).unwrap())`

            reachable(x) :- seed_reachable(x).
            reachable(y) :- reachable(x), edges(x, y).
            "#
        );
    }

    #[test]
    fn single_column_program() {
        test_snapshots!(
            r#"
            .input in1 `source_stream(in1)`
            .input in2 `source_stream(in2)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x) :- in1(x), in2(x).
            "#
        );
    }

    #[test]
    fn triple_relation_join() {
        test_snapshots!(
            r#"
            .input in1 `source_stream(in1)`
            .input in2 `source_stream(in2)`
            .input in3 `source_stream(in3)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(d, c, b, a) :- in1(a, b), in2(b, c), in3(c, d).
            "#
        );
    }

    #[test]
    fn local_constraints() {
        test_snapshots!(
            r#"
            .input input `source_stream(input)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, x) :- input(x, x).
            "#
        );

        test_snapshots!(
            r#"
            .input input `source_stream(input)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, x, y, y) :- input(x, x, y, y).
            "#
        );
    }

    #[test]
    fn test_simple_filter() {
        test_snapshots!(
            r#"
            .input input `source_stream(input)`
            .output out `for_each(|v| out.send(v).unwrap())`

            out(x, y) :- input(x, y), ( x > y ), ( y == x ).
            "#
        );
    }

    #[test]
    fn test_anti_join() {
        test_snapshots!(
            r#"
            .input ints_1 `source_stream(ints_1)`
            .input ints_2 `source_stream(ints_2)`
            .input ints_3 `source_stream(ints_3)`
            .output result `for_each(|v| result.send(v).unwrap())`

            result(x, z) :- ints_1(x, y), ints_2(y, z), !ints_3(y)
            "#
        );
    }

    #[test]
    fn test_max() {
        test_snapshots!(
            r#"
            .input ints `source_stream(ints)`
            .output result `for_each(|v| result.send(v).unwrap())`

            result(max(a), b) :- ints(a, b)
            "#
        );
    }

    #[test]
    fn test_max_all() {
        test_snapshots!(
            r#"
            .input ints `source_stream(ints)`
            .output result `for_each(|v| result.send(v).unwrap())`

            result(max(a), max(b)) :- ints(a, b)
            "#
        );
    }

    #[test]
    fn test_send_to_node() {
        test_snapshots!(
            r#"
            .input ints `source_stream(ints)`
            .output result `for_each(|v| result.send(v).unwrap())`
            .async result `for_each(|(node, data)| async_send_result(node, data))` `source_stream(async_receive_result)`

            result@b(a) :~ ints(a, b)
            "#
        );
    }

    #[test]
    fn test_aggregations_and_comments() {
        test_snapshots!(
            r#"
            # david doesn't think this line of code will execute
            .input ints `source_stream(ints)`
            .output result `for_each(|v| result.send(v).unwrap())`
            .output result2 `for_each(|v| result2.send(v).unwrap())`

            result(count(a), b) :- ints(a, b)
            result(sum(a), b) :+ ints(a, b)
            result2(choose(a), b) :- ints(a, b)
            "#
        );
    }

    #[test]
    fn test_non_copy_but_clone() {
        test_snapshots!(
            r#"
            .input strings `source_stream(strings)`
            .output result `for_each(|v| result.send(v).unwrap())`

            result(a, a) :- strings(a)
            "#
        );
    }

    #[test]
    fn test_expr_lhs() {
        test_snapshots!(
            r#"
            .input ints `source_stream(ints)`
            .output result `for_each(|v| result.send(v).unwrap())`

            result(123) :- ints(a)
            result(a + 123) :- ints(a)
            result(a + a) :- ints(a)
            result(123 - a) :- ints(a)
            "#
        );
    }
}
