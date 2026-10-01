use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use globset::{GlobBuilder, GlobMatcher};
use lsp_types::{GlobPattern, OneOf, RegistrationParams, UnregistrationParams};
use serde::Deserialize;
use serde_json::Value;

use super::types::JsonRpcError;

const CAPABILITIES: &[(&str, &str)] = &[
    ("textDocument/hover", "hoverProvider"),
    ("textDocument/definition", "definitionProvider"),
    ("textDocument/typeDefinition", "typeDefinitionProvider"),
    ("textDocument/implementation", "implementationProvider"),
    ("textDocument/references", "referencesProvider"),
    ("textDocument/rename", "renameProvider"),
    ("textDocument/completion", "completionProvider"),
    ("textDocument/signatureHelp", "signatureHelpProvider"),
    ("textDocument/documentSymbol", "documentSymbolProvider"),
    ("workspace/symbol", "workspaceSymbolProvider"),
    ("textDocument/formatting", "documentFormattingProvider"),
    (
        "textDocument/rangeFormatting",
        "documentRangeFormattingProvider",
    ),
    ("textDocument/codeAction", "codeActionProvider"),
    ("textDocument/prepareCallHierarchy", "callHierarchyProvider"),
    ("textDocument/inlayHint", "inlayHintProvider"),
    ("textDocument/diagnostic", "diagnosticProvider"),
];

fn capability_for_method(method: &str) -> Option<&'static str> {
    CAPABILITIES
        .iter()
        .find_map(|(name, capability)| (*name == method).then_some(*capability))
}

fn invalid_params(message: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError {
        code: -32602,
        message: message.to_string(),
        data: None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationOptions {
    #[serde(default)]
    document_selector: Option<Vec<DocumentFilter>>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentFilter {
    language: Option<String>,
    scheme: Option<String>,
    pattern: Option<GlobPattern>,
}

#[derive(Debug)]
struct Filter {
    language: Option<String>,
    scheme: Option<String>,
    pattern: Option<(Option<PathBuf>, GlobMatcher)>,
}

#[derive(Debug)]
struct Scope {
    filters: Option<Vec<Filter>>,
    id: Option<String>,
}

impl Scope {
    fn compile(options: Value) -> Result<Self, JsonRpcError> {
        let options: RegistrationOptions =
            serde_json::from_value(options).map_err(invalid_params)?;
        let filters = options
            .document_selector
            .map(|filters| {
                filters
                    .into_iter()
                    .map(|filter| {
                        if filter.language.is_none()
                            && filter.scheme.is_none()
                            && filter.pattern.is_none()
                        {
                            return Err(invalid_params(
                                "document filter must specify language, scheme or pattern",
                            ));
                        }
                        let pattern = filter
                            .pattern
                            .map(|pattern| {
                                let (base, pattern) = match pattern {
                                    GlobPattern::String(pattern) => (None, pattern),
                                    GlobPattern::Relative(relative) => {
                                        let uri = match relative.base_uri {
                                            OneOf::Left(folder) => folder.uri,
                                            OneOf::Right(uri) => uri,
                                        };
                                        let base = url::Url::parse(uri.as_str())
                                            .map_err(invalid_params)?
                                            .to_file_path()
                                            .map_err(|()| {
                                                invalid_params(
                                                    "relative selector base must be a file URI",
                                                )
                                            })?;
                                        (Some(base), relative.pattern)
                                    }
                                };
                                let glob = GlobBuilder::new(&pattern)
                                    .literal_separator(true)
                                    .backslash_escape(false)
                                    .build()
                                    .map_err(invalid_params)?
                                    .compile_matcher();
                                Ok((base, glob))
                            })
                            .transpose()?;
                        Ok(Filter {
                            language: filter.language,
                            scheme: filter.scheme,
                            pattern,
                        })
                    })
                    .collect::<Result<Vec<_>, JsonRpcError>>()
            })
            .transpose()?;
        Ok(Self {
            filters,
            id: options.id,
        })
    }

    fn matches(&self, document: Option<(&Path, &str)>) -> bool {
        let Some(filters) = &self.filters else {
            return true;
        };
        filters.iter().any(|filter| {
            let Some((path, language)) = document else {
                return true;
            };
            if filter
                .language
                .as_deref()
                .is_some_and(|value| value != language)
                || filter
                    .scheme
                    .as_deref()
                    .is_some_and(|value| value != "file")
            {
                return false;
            }
            filter.pattern.as_ref().is_none_or(|(base, glob)| {
                let path = if let Some(base) = base {
                    let Ok(path) = path.strip_prefix(base) else {
                        return false;
                    };
                    path
                } else {
                    path
                };
                glob.is_match(path.to_string_lossy().replace('\\', "/"))
            })
        })
    }
}

#[derive(Debug, Default)]
struct State {
    dynamic: HashMap<(&'static str, String), Scope>,
    initial: HashMap<String, Scope>,
    removed: HashSet<(&'static str, String)>,
}

#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    state: Mutex<State>,
}

impl CapabilityRegistry {
    pub(crate) fn initialize(&self, result: &Value) -> Result<(), JsonRpcError> {
        let providers = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_params("initialize result must contain capabilities"))?;
        let initial = providers
            .iter()
            .filter(|(name, value)| {
                matches!(
                    name.as_str(),
                    "typeDefinitionProvider"
                        | "implementationProvider"
                        | "callHierarchyProvider"
                        | "inlayHintProvider"
                        | "diagnosticProvider"
                ) && value.is_object()
            })
            .map(|(name, value)| Ok((name.clone(), Scope::compile(value.clone())?)))
            .collect::<Result<HashMap<_, _>, JsonRpcError>>()?;
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .initial = initial;
        Ok(())
    }

    pub(crate) fn update(&self, method: &str, params: Option<&Value>) -> Result<(), JsonRpcError> {
        match method {
            "client/registerCapability" => {
                let params: RegistrationParams =
                    serde_json::from_value(params.cloned().unwrap_or(Value::Null))
                        .map_err(invalid_params)?;
                let entries = params
                    .registrations
                    .into_iter()
                    .filter_map(|entry| {
                        capability_for_method(&entry.method).map(|capability| {
                            Scope::compile(
                                entry
                                    .register_options
                                    .unwrap_or_else(|| serde_json::json!({})),
                            )
                            .map(|scope| ((capability, entry.id), scope))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                for (key, scope) in entries {
                    state.removed.remove(&key);
                    state.dynamic.insert(key, scope);
                }
            }
            "client/unregisterCapability" => {
                let params: UnregistrationParams =
                    serde_json::from_value(params.cloned().unwrap_or(Value::Null))
                        .map_err(invalid_params)?;
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                for entry in params.unregisterations {
                    if let Some(capability) = capability_for_method(&entry.method) {
                        let key = (capability, entry.id);
                        state.dynamic.remove(&key);
                        state.removed.insert(key);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn supports(
        &self,
        capability: &str,
        document: Option<(&Path, &str)>,
        initial_supported: bool,
    ) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let initial = initial_supported
            && state.initial.get(capability).is_none_or(|scope| {
                let removed = scope.id.as_ref().is_some_and(|id| {
                    state
                        .removed
                        .iter()
                        .any(|(name, removed)| *name == capability && removed == id)
                });
                !removed && scope.matches(document)
            });
        initial
            || state
                .dynamic
                .iter()
                .any(|((name, _), scope)| *name == capability && scope.matches(document))
    }
}
