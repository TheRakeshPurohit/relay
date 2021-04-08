/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{collections::HashSet, sync::Arc};

use common::{PerfLogger, SourceLocationKey};
use graphql_ir::{
    build_ir_with_extra_features, BuilderOptions, ExecutableDefinition, FragmentDefinition,
    FragmentVariablesSemantic, OperationDefinition, Program, Selection,
};
use graphql_syntax::parse_executable_with_error_recovery;
use graphql_text_printer::print_full_operation;
use interner::{Intern, StringKey};
use lsp_types::request::Request;
use relay_compiler::{
    apply_transforms,
    config::{Config, ProjectConfig},
    Programs,
};
use schema::SDLSchema;
use serde_json::Value;

use crate::{
    lsp_extra_data_provider::Query,
    lsp_runtime_error::LSPRuntimeResult,
    server::{LSPState, SourcePrograms},
};
use serde::{Deserialize, Serialize};

pub(crate) enum GraphQLExecuteQuery {}

#[derive(Serialize, Deserialize)]
pub(crate) struct GraphQLExecuteQueryParams {
    text: String,
    variables: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum GraphQLResponse {
    #[serde(rename = "data")]
    Data(serde_json::Value),
    #[serde(rename = "error")]
    Error(serde_json::Value),
}

impl Request for GraphQLExecuteQuery {
    type Params = GraphQLExecuteQueryParams;
    type Result = GraphQLResponse;
    const METHOD: &'static str = "graphql/executeQuery";
}

/// This function will return the program that contains only operation
/// and all referenced fragments.
/// We can use it to print the full query text
fn get_operation_only_program(
    operation: Arc<OperationDefinition>,
    fragments: Vec<Arc<FragmentDefinition>>,
    project_name: StringKey,
    programs: &SourcePrograms,
) -> Option<Program> {
    let full_program = programs.get(&project_name)?;

    let mut selections_to_visit: Vec<_> = vec![&operation.selections];
    let mut next_program = Program::new(full_program.schema.clone());
    next_program.insert_operation(Arc::clone(&operation));
    for fragment in fragments.iter() {
        selections_to_visit.push(&fragment.selections);
        next_program.insert_fragment(Arc::clone(fragment));
    }

    let mut visited_fragments: HashSet<StringKey> = HashSet::default();

    while !selections_to_visit.is_empty() {
        let current_selections = selections_to_visit.pop()?;
        for selection in current_selections {
            match selection {
                Selection::FragmentSpread(spread) => {
                    // Skip, if we already visited this fragment
                    if visited_fragments.contains(&spread.fragment.item) {
                        continue;
                    }
                    visited_fragments.insert(spread.fragment.item);
                    // Second, if this fragment is already in the `next_program`,
                    // it selection already added to the visiting stack
                    if next_program.fragment(spread.fragment.item).is_some() {
                        continue;
                    }

                    // Finally, add all missing fragment spreads from the full program
                    let fragment = full_program
                        .fragment(spread.fragment.item)
                        .expect("Expect fragment to exist");
                    selections_to_visit.push(&fragment.selections);
                    next_program.insert_fragment(Arc::clone(fragment));
                }
                Selection::Condition(condition) => {
                    selections_to_visit.push(&condition.selections);
                }
                Selection::LinkedField(linked_field) => {
                    selections_to_visit.push(&linked_field.selections);
                }
                Selection::InlineFragment(inline_fragment) => {
                    selections_to_visit.push(&inline_fragment.selections);
                }
                Selection::ScalarField(_) => {}
            }
        }
    }

    Some(next_program)
}

/// Given the `Program` that contain operation+all its fragments this
/// function will `apply_transforms` and create a full set of programs, including the one
/// that may generate full operation text
fn transform_program<TPerfLogger: PerfLogger + 'static>(
    project_config: &ProjectConfig,
    config: Arc<Config>,
    program: Arc<Program>,
    perf_logger: Arc<TPerfLogger>,
) -> Result<Programs, String> {
    apply_transforms(
        project_config.name,
        program,
        Default::default(),
        &config.connection_interface,
        Arc::new(
            project_config
                .feature_flags
                .as_ref()
                .cloned()
                .unwrap_or_else(|| config.feature_flags.clone()),
        ),
        perf_logger,
    )
    .map_err(|errors| format!("{:?}", errors))
}

fn print_full_operation_text(programs: Programs, operation_name: StringKey) -> String {
    let print_operation_node = programs
        .operation_text
        .operation(operation_name)
        .expect("a query text operation should be generated for this operation");

    print_full_operation(&programs.operation_text, print_operation_node)
}

/// From the list of AST nodes we're trying to extract the operation and possible
/// list of fragment, to construct the initial `Program` that we could later transform
/// and print
fn build_operation_ir_with_fragments(
    definitions: &[graphql_syntax::ExecutableDefinition],
    schema: Arc<SDLSchema>,
) -> Result<(Arc<OperationDefinition>, Vec<Arc<FragmentDefinition>>), String> {
    let ir = build_ir_with_extra_features(
        &schema,
        definitions,
        BuilderOptions {
            allow_undefined_fragment_spreads: true,
            fragment_variables_semantic: FragmentVariablesSemantic::PassedValue,
            relay_mode: true,
            default_anonymous_operation_name: Some("anonymous".intern()),
        },
    )
    .map_err(|errors| format!("{:?}", errors))?;

    if let Some(operation) = ir.iter().find_map(|item| {
        if let ExecutableDefinition::Operation(operation) = item {
            Some(Arc::new(operation.clone()))
        } else {
            None
        }
    }) {
        let fragments = ir
            .iter()
            .filter_map(|item| {
                if let ExecutableDefinition::Fragment(fragment) = item {
                    Some(Arc::new(fragment.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok((operation, fragments))
    } else {
        Err("Unable to find an operation.".to_string())
    }
}

fn get_query_text<TPerfLogger: PerfLogger + 'static>(
    state: &LSPState<TPerfLogger>,
    original_text: String,
    project_config: &ProjectConfig,
    schema: Arc<SDLSchema>,
) -> Result<String, String> {
    let result = parse_executable_with_error_recovery(&original_text, SourceLocationKey::Generated);

    if !&result.errors.is_empty() {
        return Err(result
            .errors
            .iter()
            .map(|err| format!("- {}\n", err))
            .collect::<String>());
    }

    let (operation, fragments) =
        build_operation_ir_with_fragments(&result.item.definitions, schema)?;

    let operation_name = operation.name.item;
    let query_text = if let Some(operation_program) = get_operation_only_program(
        operation,
        fragments,
        project_config.name,
        state.get_source_programs_ref(),
    ) {
        let programs = transform_program(
            project_config,
            state.get_config(),
            Arc::new(operation_program),
            state.get_logger(),
        )?;

        print_full_operation_text(programs, operation_name)
    } else {
        // If, for some reason we could not get the operation text from the sources
        // we will send the original text to the server
        original_text
    };

    Ok(query_text)
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn on_graphql_execute_query<TPerfLogger: PerfLogger + 'static>(
    state: &mut LSPState<TPerfLogger>,
    params: GraphQLExecuteQueryParams,
) -> LSPRuntimeResult<<GraphQLExecuteQuery as Request>::Result> {
    let project_name = "facebook".intern();
    let project_config = state.get_project_config_ref(project_name).unwrap();
    let schema = state
        .get_schemas()
        .get(&project_config.name)
        .unwrap()
        .clone();

    let query_text = match get_query_text(state, params.text, project_config, schema) {
        Ok(query_text) => query_text,
        Err(error) => return Ok(GraphQLResponse::Error(Value::String(error))),
    };

    match state
        .extra_data_provider
        .execute_query(Query::Text(query_text), params.variables)
    {
        Ok(data) => Ok(GraphQLResponse::Data(data)),
        Err(error) => Ok(GraphQLResponse::Error(error)),
    }
}