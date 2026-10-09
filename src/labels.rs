//! Completion and symbol labels for the IntelliJ server.
//!
//! The server describes items through `labelDetails` rather than `detail`:
//! - methods: `detail` is `(params)`, `description` the return type;
//! - fields, constants, enum members and locals: `description` is the type and
//!   `detail` extra context such as ` ( = 3) (com.example)`;
//! - classes: `detail` is `<generics> package`;
//! - constructors: `detail` is `<generics> package(params)`;
//! - postfix templates have no kind, and `description` shows the expansion.
//!
//! Labels follow upstream's JDTLS layout (`name(params) : Type`), with the
//! code parts highlighted as Java and the context dimmed.

use zed_extension_api::{
    CodeLabel, CodeLabelSpan,
    lsp::{Completion, CompletionKind, Symbol, SymbolKind},
};

const CONTEXT_HIGHLIGHT: &str = "comment";

pub fn label_for_completion(completion: &Completion) -> Option<CodeLabel> {
    let details = completion.label_details.as_ref();
    completion_label(
        &completion.label,
        completion.kind,
        details
            .and_then(|details| details.detail.as_deref())
            .unwrap_or(""),
        details
            .and_then(|details| details.description.as_deref())
            .unwrap_or(""),
    )
}

fn completion_label(
    label: &str,
    kind: Option<CompletionKind>,
    detail: &str,
    description: &str,
) -> Option<CodeLabel> {
    let Some(kind) = kind else {
        // Postfix and live templates: `nn - if (expr != null)`.
        return (!description.is_empty()).then(|| CodeLabel {
            code: String::new(),
            spans: vec![
                CodeLabelSpan::literal(label, None),
                CodeLabelSpan::literal(format!(" {description}"), context()),
            ],
            filter_range: (0..label.len()).into(),
        });
    };

    match kind {
        CompletionKind::Method | CompletionKind::Function => {
            let return_type = non_empty(description).unwrap_or("void");
            let parameters = non_empty(detail).unwrap_or("()");
            // `Ret name(params) {}`
            let code = format!("{return_type} {label}{parameters} {{}}");
            let name_start = return_type.len() + 1;
            Some(CodeLabel {
                spans: vec![
                    CodeLabelSpan::code_range(
                        name_start..name_start + label.len() + parameters.len(),
                    ),
                    CodeLabelSpan::literal(" : ", None),
                    CodeLabelSpan::code_range(0..return_type.len()),
                ],
                code,
                filter_range: (0..label.len()).into(),
            })
        }
        CompletionKind::Field
        | CompletionKind::Constant
        | CompletionKind::Variable
        | CompletionKind::EnumMember
        | CompletionKind::Property => {
            let field_type = non_empty(description)?;
            // `Type name;`
            let code = format!("{field_type} {label};");
            let name_start = field_type.len() + 1;
            let mut spans = vec![
                CodeLabelSpan::code_range(name_start..name_start + label.len()),
                CodeLabelSpan::literal(" : ", None),
                CodeLabelSpan::code_range(0..field_type.len()),
            ];
            if let Some(extra) = non_empty(detail.trim()) {
                spans.push(CodeLabelSpan::literal(format!(" {extra}"), context()));
            }
            Some(CodeLabel {
                spans,
                code,
                filter_range: (0..label.len()).into(),
            })
        }
        CompletionKind::Class
        | CompletionKind::Interface
        | CompletionKind::Enum
        | CompletionKind::Struct
        | CompletionKind::TypeParameter => {
            let keyword = match kind {
                CompletionKind::Interface => "interface ",
                CompletionKind::Enum => "enum ",
                CompletionKind::Struct => "record ",
                _ => "class ",
            };
            let (generics, container) = split_generics(detail);
            // `class Name<E> {}`
            let code = format!("{keyword}{label}{generics} {{}}");
            let mut spans = vec![CodeLabelSpan::code_range(
                keyword.len()..keyword.len() + label.len() + generics.len(),
            )];
            if let Some(container) = non_empty(container) {
                spans.push(CodeLabelSpan::literal(format!(" ({container})"), context()));
            }
            Some(CodeLabel {
                spans,
                code,
                filter_range: (0..label.len()).into(),
            })
        }
        CompletionKind::Constructor => {
            let (generics, rest) = split_generics(detail);
            let (container, parameters) = rest
                .find('(')
                .map(|start| (&rest[..start], &rest[start..]))
                .unwrap_or((rest, "()"));
            // `class Name<E> { Name(params) {} }`, highlighting `Name(params)`.
            let class_open = format!("class {label}{generics} {{ ");
            let code = format!("{class_open}{label}{parameters} {{}} }}");
            let mut spans = vec![CodeLabelSpan::code_range(
                class_open.len()..class_open.len() + label.len() + parameters.len(),
            )];
            if let Some(container) = non_empty(container.trim()) {
                spans.push(CodeLabelSpan::literal(format!(" ({container})"), context()));
            }
            Some(CodeLabel {
                spans,
                code,
                filter_range: (0..label.len()).into(),
            })
        }
        CompletionKind::Keyword => Some(CodeLabel {
            spans: vec![CodeLabelSpan::code_range(0..label.len())],
            filter_range: (0..label.len()).into(),
            code: label.to_string(),
        }),
        _ => None,
    }
}

/// Symbol names from this server are bare (`Main`, `area`); the package comes
/// separately as the container, which Zed doesn't pass to extensions.
pub fn label_for_symbol(symbol: &Symbol) -> Option<CodeLabel> {
    let name = symbol.name.as_str();
    let (prefix, suffix) = match symbol.kind {
        SymbolKind::Class => ("class ", " {}"),
        SymbolKind::Interface => ("interface ", " {}"),
        SymbolKind::Enum => ("enum ", " {}"),
        SymbolKind::Struct => ("record ", "() {}"),
        SymbolKind::Method | SymbolKind::Function | SymbolKind::Constructor => {
            ("class _ { void ", "() {} }")
        }
        SymbolKind::Field | SymbolKind::Constant | SymbolKind::EnumMember => {
            ("class _ { Object ", "; }")
        }
        _ => return None,
    };
    Some(CodeLabel {
        code: format!("{prefix}{name}{suffix}"),
        spans: vec![CodeLabelSpan::code_range(
            prefix.len()..prefix.len() + name.len(),
        )],
        filter_range: (0..name.len()).into(),
    })
}

/// Split `<E> java.util` into `("<E>", "java.util")`.
fn split_generics(detail: &str) -> (&str, &str) {
    let detail = detail.trim_start();
    if detail.starts_with('<') {
        let mut depth = 0;
        for (index, c) in detail.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        return (&detail[..=index], detail[index + 1..].trim_start());
                    }
                }
                _ => {}
            }
        }
    }
    ("", detail)
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn context() -> Option<String> {
    Some(CONTEXT_HIGHLIGHT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item<'a>(&'a str, Option<CompletionKind>, &'a str, &'a str);

    fn completion<'a>(
        label: &'a str,
        kind: Option<CompletionKind>,
        detail: &'a str,
        description: &'a str,
    ) -> Item<'a> {
        Item(label, kind, detail, description)
    }

    /// Render the label as Zed shows it.
    fn text(label: &CodeLabel) -> String {
        label
            .spans
            .iter()
            .map(|span| match span {
                CodeLabelSpan::CodeRange(range) => {
                    label.code[range.start as usize..range.end as usize].to_string()
                }
                CodeLabelSpan::Literal(literal) => literal.text.clone(),
            })
            .collect()
    }

    fn label(item: &Item) -> Option<CodeLabel> {
        completion_label(item.0, item.1, item.2, item.3)
    }

    fn render(item: Item) -> String {
        let label = label(&item).expect("label");
        let rendered = text(&label);
        let filter = label.filter_range.start as usize..label.filter_range.end as usize;
        assert_eq!(
            &rendered[filter], item.0,
            "filter range must cover the name"
        );
        rendered
    }

    #[test]
    fn methods_show_parameters_and_return_type() {
        assert_eq!(
            render(completion(
                "setIn",
                Some(CompletionKind::Method),
                "(InputStream in)",
                "void"
            )),
            "setIn(InputStream in) : void"
        );
        assert_eq!(
            render(completion(
                "size",
                Some(CompletionKind::Method),
                "()",
                "int"
            )),
            "size() : int"
        );
    }

    #[test]
    fn fields_show_type_and_context() {
        assert_eq!(
            render(completion(
                "LIMIT",
                Some(CompletionKind::Field),
                " ( = 3) (com.example)",
                "int"
            )),
            "LIMIT : int ( = 3) (com.example)"
        );
        assert_eq!(
            render(completion(
                "localCount",
                Some(CompletionKind::Variable),
                "",
                "int"
            )),
            "localCount : int"
        );
        assert_eq!(
            render(completion(
                "RED",
                Some(CompletionKind::EnumMember),
                " (com.example.Main)",
                "Color"
            )),
            "RED : Color (com.example.Main)"
        );
    }

    #[test]
    fn classes_show_generics_and_package() {
        assert_eq!(
            render(completion(
                "ArrayList",
                Some(CompletionKind::Class),
                "<E> java.util",
                ""
            )),
            "ArrayList<E> (java.util)"
        );
        assert_eq!(
            render(completion(
                "String",
                Some(CompletionKind::Class),
                " java.lang",
                ""
            )),
            "String (java.lang)"
        );
        assert_eq!(
            render(completion(
                "Map",
                Some(CompletionKind::Interface),
                "<K, V> java.util",
                ""
            )),
            "Map<K, V> (java.util)"
        );
    }

    #[test]
    fn constructors_show_parameters_and_package() {
        assert_eq!(
            render(completion(
                "ArrayNodeListIterator",
                Some(CompletionKind::Constructor),
                " com.sun.org.apache.xalan.internal.xsltc.dom(int[] nodes)",
                ""
            )),
            "ArrayNodeListIterator(int[] nodes) (com.sun.org.apache.xalan.internal.xsltc.dom)"
        );
        assert_eq!(
            render(completion(
                "ArrayList",
                Some(CompletionKind::Constructor),
                "<E> java.util(int initialCapacity)",
                ""
            )),
            "ArrayList(int initialCapacity) (java.util)"
        );
    }

    #[test]
    fn keywords_and_templates() {
        assert_eq!(
            render(completion("return", Some(CompletionKind::Keyword), "", "")),
            "return"
        );
        assert_eq!(
            render(completion("nn", None, "", "if (expr != null)")),
            "nn if (expr != null)"
        );
    }

    #[test]
    fn items_without_details_fall_back_to_zed() {
        assert!(label(&completion("x", Some(CompletionKind::Field), "", "")).is_none());
        assert!(label(&completion("x", None, "", "")).is_none());
    }

    #[test]
    fn generics_split_handles_nesting() {
        assert_eq!(
            split_generics("<T extends Comparable<T>> java.util"),
            ("<T extends Comparable<T>>", "java.util")
        );
        assert_eq!(split_generics(" java.lang"), ("", "java.lang"));
    }

    #[test]
    fn symbols_highlight_the_name() {
        let label = label_for_symbol(&Symbol {
            kind: SymbolKind::Class,
            name: "Main".to_string(),
        })
        .unwrap();
        assert_eq!(text(&label), "Main");
        assert_eq!(&label.code, "class Main {}");
    }
}
