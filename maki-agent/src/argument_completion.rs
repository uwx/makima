//! `/model` argument completion, shared by the TUI palette and ACP clients:
//! specs, then the `key=value` selection fields the model still accepts.

use std::ops::Range;
use std::sync::Arc;

use maki_commands::{ArgumentValue, CompletionItem, ParsedArgument};
use maki_providers::{ModelCapabilities, ModelOption};

const FIELD_OPEN: char = '{';
const FIELD_CLOSE: char = '}';
const FIELD_SEPARATOR: char = ',';
const FIELD_ASSIGNMENT: char = '=';

/// What one `/model` argument accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArgumentPlan {
    /// The model named before the field list or by an earlier argument.
    pub named: Option<Arc<str>>,
    pub offer_specs: bool,
    pub options: Vec<ModelOption>,
}

pub fn plan_model_argument(
    arguments: &str,
    argument: &str,
    argument_range: Option<&Range<usize>>,
    preceding: &[ParsedArgument],
) -> ModelArgumentPlan {
    let preceding = preceding
        .iter()
        .flat_map(|argument| argument.values.iter())
        .filter_map(|value| match value {
            ArgumentValue::String(value) => Some(value.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let open_brace = argument_range
        .and_then(|range| arguments.get(..range.start))
        .and_then(|head| head.rfind(FIELD_OPEN));
    let named = match open_brace {
        Some(open) => arguments
            .get(..open)
            .and_then(|head| head.rsplit(char::is_whitespace).next())
            .map(|token| token.trim_matches('"')),
        None => preceding
            .iter()
            .copied()
            .find(|value| !value.contains(FIELD_ASSIGNMENT)),
    }
    .filter(|argument| !argument.is_empty())
    .map(Arc::from);
    let offer_specs =
        named.is_none() && open_brace.is_none() && !argument.contains(FIELD_ASSIGNMENT);
    let embedded = open_brace.map(|open| {
        let end = arguments[open..]
            .find(FIELD_CLOSE)
            .map_or(arguments.len(), |end| open + end);
        (open + 1, end)
    });
    let options = ModelOption::ALL
        .into_iter()
        .filter(|option| {
            let prefix = format!("{}=", option.key());
            let duplicate = embedded.map_or_else(
                || preceding.iter().any(|value| value.starts_with(&prefix)),
                |(start, end)| {
                    arguments[start..end]
                        .split(FIELD_SEPARATOR)
                        .scan(start, |offset, option| {
                            let option_start = *offset;
                            *offset += option.len() + 1;
                            Some((option_start, option))
                        })
                        .any(|(start, option)| {
                            let start = start + option.len() - option.trim_start().len();
                            argument_range.is_none_or(|range| start != range.start)
                                && option.trim_start().starts_with(&prefix)
                        })
                },
            );
            !duplicate && (!argument.contains(FIELD_ASSIGNMENT) || argument.starts_with(&prefix))
        })
        .collect();
    ModelArgumentPlan {
        named,
        offer_specs,
        options,
    }
}

/// The visible candidates: the spec list when nothing names a model yet, then
/// the presets of every option the model supports. `unconstrained` covers
/// hosts that cannot say which model is current, so they offer every preset.
pub fn model_argument_items(
    specs: &[Arc<str>],
    plan: &ModelArgumentPlan,
    capabilities: Option<ModelCapabilities>,
    unconstrained: bool,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    if plan.offer_specs {
        items.extend(specs.iter().map(|spec| CompletionItem {
            label: Arc::clone(spec),
            insertion: Arc::clone(spec),
            description: None,
        }));
    }
    for option in &plan.options {
        let supported =
            capabilities.map_or(unconstrained, |capabilities| capabilities.supports(*option));
        if !supported {
            continue;
        }
        items.extend(option.presets().iter().map(|value| {
            let insertion: Arc<str> = Arc::from(format!("{}={value}", option.key()));
            CompletionItem {
                label: Arc::clone(&insertion),
                insertion,
                description: Some(Arc::from(option.description())),
            }
        }));
    }
    items
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use maki_commands::{ArgumentValue, CompletionItem, ParsedArgument};

    use super::{model_argument_items, plan_model_argument};

    fn preceding(values: &[&str]) -> Vec<ParsedArgument> {
        vec![ParsedArgument {
            name: Arc::from("model"),
            values: values
                .iter()
                .map(|value| ArgumentValue::String(Arc::from(*value)))
                .collect(),
            variadic: true,
        }]
    }

    fn insertions(items: &[CompletionItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| item.insertion.to_string())
            .collect()
    }

    fn offered(arguments: &str, argument: &str, earlier: &[&str]) -> Vec<String> {
        let plan = plan_model_argument(arguments, argument, None, &preceding(earlier));
        insertions(&model_argument_items(&[], &plan, None, true))
    }

    #[test]
    fn an_empty_argument_offers_both_fields() {
        let values = offered("", "", &[]);
        assert!(values.iter().any(|value| value.starts_with("thinking=")));
        assert!(values.iter().any(|value| value.starts_with("fast=")));
    }

    #[test]
    fn a_selected_field_is_not_offered_twice() {
        let values = offered("fast=true ", "", &["fast=true"]);
        assert!(values.iter().all(|value| !value.starts_with("fast=")));
        assert!(values.iter().any(|value| value.starts_with("thinking=")));
    }

    #[test]
    fn a_field_prefix_narrows_the_presets() {
        let values = offered("thinking=", "thinking=", &[]);
        assert!(values.iter().all(|value| value.starts_with("thinking=")));
    }
}
