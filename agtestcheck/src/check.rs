use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::source_map::Comment;
use crate::source_map::ModuleKind;
use crate::source_map::SourceMap;
use crate::source_map::SourceType;
use crate::source_map::SourceTypeKind;

const STEPS: [&str; 4] = ["GIVEN", "WHEN", "THEN", "AND"];
const WHY: &str = "WHY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    pub(crate) file: PathBuf,
    /// 0 for issues of the whole file.
    pub(crate) line: usize,
    pub(crate) kind: IssueKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueKind {
    Unparsable {
        error: String,
    },
    /// Not `<subject>[_<args>]_should_<text>` with the module name as the
    /// subject.
    TestName {
        test: String,
        subject: String,
    },
    TestPlacement {
        test: String,
    },
    /// A test of a method placed directly in the module of its type.
    TestModule {
        test: String,
        module: String,
    },
    /// A cross-method test after the method modules of its type.
    TestOrder {
        test: String,
    },
    UnknownModule {
        module: String,
    },
    /// A module of a trait method placed directly in the module of the type,
    /// `path` is where it belongs from the root test module, `direct` when
    /// the trait has a single method so its trait module can hold the tests.
    TraitMethodModule {
        module: String,
        path: String,
        direct: bool,
    },
    Comment {
        text: String,
    },
    CommentOrder {
        text: String,
    },
}

impl std::fmt::Display for IssueKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueKind::Unparsable { error } => write!(f, "cannot parse the file: {error}"),
            IssueKind::TestName { test, subject } => write!(
                f,
                "test `{test}` should be named `{subject}_should_<behaviour>` or `{subject}_<args>_should_<behaviour>`"
            ),
            IssueKind::TestPlacement { test } => write!(
                f,
                "test `{test}` should be in the module of its type, trait, function or method"
            ),
            IssueKind::TestModule { test, module } => {
                write!(f, "test `{test}` should be in module `{module}`")
            }
            IssueKind::TestOrder { test } => write!(
                f,
                "cross-method test `{test}` should come before the method modules"
            ),
            IssueKind::TraitMethodModule {
                module,
                path,
                direct,
            } => {
                let trait_module = path
                    .rsplit_once("::")
                    .filter(|_| *direct)
                    .map(|(trait_module, _)| format!("`{trait_module}` or "))
                    .unwrap_or_default();
                write!(
                    f,
                    "module `{module}` tests a trait method and should be {trait_module}`{path}`"
                )
            }
            IssueKind::UnknownModule { module } => write!(
                f,
                "module `{module}` does not match any type, trait, function or method"
            ),
            IssueKind::Comment { text } => write!(
                f,
                "comment \"{}\" should be deleted, start with WHY or GIVEN, WHEN, THEN, AND",
                text.lines().next().unwrap_or_default()
            ),
            IssueKind::CommentOrder { text } => write!(
                f,
                "step \"{text}\" is out of order: AND cannot come first and GIVEN cannot follow WHEN or THEN"
            ),
        }
    }
}

pub(crate) fn check(map: &SourceMap) -> Vec<Issue> {
    let mut issues: Vec<Issue> = map
        .errors
        .iter()
        .map(|(file, error)| Issue {
            file: file.clone(),
            line: 0,
            kind: IssueKind::Unparsable {
                error: error.clone(),
            },
        })
        .collect();

    let mut traits = Vec::new();
    let mut files: Vec<_> = map.files.iter().collect();
    files.sort_by_key(|(file, _)| *file);
    let mut crates: HashMap<Option<&Path>, Vec<SourceType>> = HashMap::new();

    for (file, types) in &files {
        trait_definitions(file, types.types.values().flatten(), &mut traits);
        definitions(
            types.types.values().flatten(),
            crates.entry(types.crate_dir.as_deref()).or_default(),
        );
    }

    for (file, types) in &files {
        let items: Vec<&SourceType> = types.types.values().flatten().collect();
        let lookup: Vec<&SourceType> = if items.iter().any(|i| has_definitions(i)) {
            items.clone()
        } else {
            crates[&types.crate_dir.as_deref()].iter().collect()
        };
        Checker {
            file,
            traits: &traits,
            issues: &mut issues,
        }
        .scope(&items, &lookup);
    }

    issues.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    issues
}

struct Checker<'a> {
    file: &'a Path,
    traits: &'a [(&'a Path, &'a SourceType)],
    issues: &'a mut Vec<Issue>,
}

impl Checker<'_> {
    fn scope(&mut self, items: &[&SourceType], lookup: &[&SourceType]) {
        for item in items {
            match item.kind {
                SourceTypeKind::Module(ModuleKind::Test) => self.tests(item, lookup),
                SourceTypeKind::Module(ModuleKind::Source) => {
                    let module_items: Vec<&SourceType> = item.types.iter().collect();
                    self.scope(&module_items, &module_items);
                }
                SourceTypeKind::Test => self.misplaced(item),
                _ => {}
            }
        }
    }

    fn tests(&mut self, module: &SourceType, scope: &[&SourceType]) {
        for child in &module.types {
            match child.kind {
                SourceTypeKind::Test => self.misplaced(child),
                SourceTypeKind::Module(ModuleKind::Test) => {
                    match root_subject(scope, &child.name) {
                        Some(subject) => self.subject(
                            child,
                            &subject.types,
                            matches!(subject.kind, SourceTypeKind::Type | SourceTypeKind::Trait)
                                .then_some(scope),
                            None,
                        ),
                        None => self.unknown(child),
                    }
                }
                _ => {}
            }
        }
    }

    /// Cross-method tests of a type must come before its method modules.
    /// `type_scope` is the scope of the type for the module of a type.
    fn subject(
        &mut self,
        module: &SourceType,
        items: &[SourceType],
        type_scope: Option<&[&SourceType]>,
        alias: Option<&str>,
    ) {
        let is_type = type_scope.is_some();
        let mut after_modules = false;

        for child in &module.types {
            match child.kind {
                SourceTypeKind::Test => {
                    if after_modules {
                        self.issue(
                            child.line,
                            IssueKind::TestOrder {
                                test: child.name.clone(),
                            },
                        );
                    }

                    match is_type
                        .then(|| method_of(items, &child.name, &module.name))
                        .flatten()
                    {
                        Some(method) => {
                            self.issue(
                                child.line,
                                IssueKind::TestModule {
                                    test: child.name.clone(),
                                    module: format!("{}::{method}", module.name),
                                },
                            );
                            self.comments(&child.comments);
                        }
                        None => self.test(child, &module.name, alias),
                    }
                }
                SourceTypeKind::Module(ModuleKind::Test) => match nested_subject(items, child) {
                    Some(subject) => {
                        after_modules |= is_type;

                        if subject.kind == SourceTypeKind::Trait {
                            let mut methods = subject.types.clone();
                            methods.extend(self.defaults(subject, type_scope.unwrap_or_default()));
                            self.trait_impl(child, &methods, &module.name);
                        } else {
                            self.subject(child, &subject.types, None, None);
                        }
                    }
                    None => match type_scope.and_then(|scope| {
                        self.trait_method_path(&module.name, items, scope, &child.name)
                    }) {
                        Some((path, direct)) => {
                            self.issue(
                                child.line,
                                IssueKind::TraitMethodModule {
                                    module: child.name.clone(),
                                    path,
                                    direct,
                                },
                            );
                            self.unresolved(child);
                        }
                        None => self.unknown(child),
                    },
                },
                _ => {}
            }
        }
    }

    /// The tests of a trait implemented for a type belong to its methods,
    /// including the default ones, unless it has none (`impl Error for E {}`).
    fn trait_impl(&mut self, module: &SourceType, items: &[SourceType], type_module: &str) {
        let methods: Vec<&SourceType> = items
            .iter()
            .filter(|m| m.kind == SourceTypeKind::Method)
            .collect();

        // With no or a single method (`impl Drop`) the tests can be in the
        // trait module directly, named after the trait or the method.
        if methods.len() <= 1 {
            let method = methods.first().map(|m| module_name(&m.name));
            return self.subject(module, items, None, method.as_deref());
        }

        for child in &module.types {
            match child.kind {
                SourceTypeKind::Test => match method_named(items, &child.name) {
                    Some(method) => {
                        self.issue(
                            child.line,
                            IssueKind::TestModule {
                                test: child.name.clone(),
                                module: format!("{type_module}::{}::{method}", module.name),
                            },
                        );
                        self.comments(&child.comments);
                    }
                    None => self.misplaced(child),
                },
                SourceTypeKind::Module(ModuleKind::Test) => match nested_subject(items, child) {
                    Some(method) => self.subject(child, &method.types, None, None),
                    None => self.unknown(child),
                },
                _ => {}
            }
        }
    }

    fn test(&mut self, test: &SourceType, subject: &str, alias: Option<&str>) {
        if !is_test_name(&test.name, subject) && !alias.is_some_and(|a| is_test_name(&test.name, a))
        {
            self.issue(
                test.line,
                IssueKind::TestName {
                    test: test.name.clone(),
                    subject: subject.to_string(),
                },
            );
        }

        self.comments(&test.comments);
    }

    /// Consecutive `//` lines are one comment, so each line starting with a
    /// keyword is a step and the other lines continue the step above them.
    fn comments(&mut self, comments: &[Comment]) {
        let mut started = false;
        let mut acted = false;

        for comment in comments {
            let mut why = false;

            for (line, text) in (comment.line..).zip(comment.text.lines()) {
                let word = text
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap_or_default();

                // A wrapped WHY can continue with "AND ..." but a new step
                // starts with GIVEN, WHEN or THEN.
                if word == WHY || (why && word == "AND") {
                    why = true;
                    continue;
                }

                if !STEPS.contains(&word) {
                    if line == comment.line {
                        self.issue(
                            line,
                            IssueKind::Comment {
                                text: comment.text.clone(),
                            },
                        );
                    }

                    continue;
                }

                why = false;
                let in_order = match word {
                    "AND" => started,
                    "GIVEN" => !acted,
                    _ => true,
                };

                if !in_order {
                    self.issue(
                        line,
                        IssueKind::CommentOrder {
                            text: text.to_string(),
                        },
                    );
                }

                started = true;
                acted |= matches!(word, "WHEN" | "THEN");
            }
        }
    }

    fn misplaced(&mut self, test: &SourceType) {
        self.issue(
            test.line,
            IssueKind::TestPlacement {
                test: test.name.clone(),
            },
        );
        self.comments(&test.comments);
    }

    /// Tests below an unknown module are still checked against its name.
    fn unknown(&mut self, module: &SourceType) {
        self.issue(
            module.line,
            IssueKind::UnknownModule {
                module: module.name.clone(),
            },
        );
        self.unresolved(module);
    }

    fn unresolved(&mut self, module: &SourceType) {
        for child in &module.types {
            match child.kind {
                SourceTypeKind::Test => self.test(child, &module.name, None),
                SourceTypeKind::Module(ModuleKind::Test) => self.unresolved(child),
                _ => {}
            }
        }
    }

    /// A method of a trait implemented for the type, overridden or default,
    /// belongs to the trait module under the type.
    fn trait_method_path(
        &self,
        type_module: &str,
        items: &[SourceType],
        scope: &[&SourceType],
        method: &str,
    ) -> Option<(String, bool)> {
        items
            .iter()
            .filter(|t| t.kind == SourceTypeKind::Trait)
            .find_map(|t| {
                let defaults = self.defaults(t, scope);
                let methods: Vec<&SourceType> = t
                    .types
                    .iter()
                    .chain(&defaults)
                    .filter(|m| m.kind == SourceTypeKind::Method)
                    .collect();

                methods.iter().any(|m| m.name == method).then(|| {
                    (
                        format!("{type_module}::{}::{method}", module_name(&t.name)),
                        methods.len() == 1,
                    )
                })
            })
    }

    /// The default methods a trait impl does not override. The trait is looked
    /// up in the file, then in other files where only a single trait of that
    /// name is used.
    fn defaults(&self, trait_impl: &SourceType, scope: &[&SourceType]) -> Vec<SourceType> {
        let name = trait_impl.name.rsplit("::").next().unwrap_or_default();
        let is_definition = |d: &SourceType| d.kind == SourceTypeKind::Trait && d.name == name;
        let definition = scope
            .iter()
            .copied()
            .find(|d| is_definition(d))
            .or_else(|| {
                let mut elsewhere = self
                    .traits
                    .iter()
                    .filter(|(file, d)| *file != self.file && is_definition(d));

                match (elsewhere.next(), elsewhere.next()) {
                    (Some((_, d)), None) => Some(*d),
                    _ => None,
                }
            });

        definition
            .into_iter()
            .flat_map(|d| &d.types)
            .filter(|m| {
                m.kind == SourceTypeKind::Method
                    && !trait_impl.types.iter().any(|i| i.name == m.name)
            })
            .cloned()
            .collect()
    }

    fn issue(&mut self, line: usize, kind: IssueKind) {
        self.issues.push(Issue {
            file: self.file.to_path_buf(),
            line,
            kind,
        });
    }
}

fn trait_definitions<'a>(
    file: &'a Path,
    items: impl IntoIterator<Item = &'a SourceType>,
    traits: &mut Vec<(&'a Path, &'a SourceType)>,
) {
    for item in items {
        match item.kind {
            SourceTypeKind::Trait => traits.push((file, item)),
            SourceTypeKind::Module(ModuleKind::Source) => {
                trait_definitions(file, &item.types, traits);
            }
            _ => {}
        }
    }
}

/// The types, traits, functions and macros of all modules. A type or trait
/// spread over files, e.g. impls in another file, is merged into one.
fn definitions<'a>(items: impl IntoIterator<Item = &'a SourceType>, defs: &mut Vec<SourceType>) {
    for item in items {
        match item.kind {
            SourceTypeKind::Type
            | SourceTypeKind::Trait
            | SourceTypeKind::Function
            | SourceTypeKind::Macro => merge(item, defs),
            SourceTypeKind::Module(ModuleKind::Source) => definitions(&item.types, defs),
            _ => {}
        }
    }
}

fn merge(item: &SourceType, defs: &mut Vec<SourceType>) {
    match defs.iter_mut().find(|d| {
        matches!(d.kind, SourceTypeKind::Type | SourceTypeKind::Trait)
            && d.kind == item.kind
            && d.name == item.name
    }) {
        Some(def) => item.types.iter().for_each(|t| merge(t, &mut def.types)),
        None => defs.push(item.clone()),
    }
}

fn has_definitions(item: &SourceType) -> bool {
    match item.kind {
        SourceTypeKind::Module(ModuleKind::Source) => item.types.iter().any(has_definitions),
        SourceTypeKind::Module(_) | SourceTypeKind::Test => false,
        _ => true,
    }
}

/// A type is preferred over a function of the same module name (`struct Foo`
/// and `fn foo`).
fn root_subject<'a>(scope: &[&'a SourceType], module: &str) -> Option<&'a SourceType> {
    let named = |is_kind: fn(&SourceTypeKind) -> bool| {
        scope
            .iter()
            .copied()
            .find(|s| is_kind(&s.kind) && module_name(&s.name) == module)
    };

    named(|k| matches!(k, SourceTypeKind::Type | SourceTypeKind::Trait))
        .or_else(|| named(|k| matches!(k, SourceTypeKind::Function | SourceTypeKind::Macro)))
}

/// The method a test named after it (`is_dir_should_...`) belongs to when it
/// is not named after its type.
fn method_of(items: &[SourceType], test: &str, subject: &str) -> Option<String> {
    if is_test_name(test, subject) {
        return None;
    }

    method_named(items, test)
}

fn method_named(items: &[SourceType], test: &str) -> Option<String> {
    items
        .iter()
        .filter(|m| m.kind == SourceTypeKind::Method)
        .map(|m| module_name(&m.name))
        .filter(|m| is_test_name(test, m))
        .max_by_key(String::len)
}

/// An inherent `fn write` and `impl Write` share a module name: nested
/// modules mean the trait, direct tests the method.
fn nested_subject<'a>(items: &'a [SourceType], module: &SourceType) -> Option<&'a SourceType> {
    let named = |kind: SourceTypeKind| {
        items
            .iter()
            .find(|s| s.kind == kind && module_name(&s.name) == module.name)
    };
    let has_modules = module
        .types
        .iter()
        .any(|t| t.kind == SourceTypeKind::Module(ModuleKind::Test));

    if has_modules {
        named(SourceTypeKind::Trait).or_else(|| named(SourceTypeKind::Method))
    } else {
        named(SourceTypeKind::Method).or_else(|| named(SourceTypeKind::Trait))
    }
}

/// Trait paths are written into the name, e.g. `fmt::Write` is `fmt_write`.
fn module_name(name: &str) -> String {
    name.split("::")
        .map(to_snake_case)
        .collect::<Vec<_>>()
        .join("_")
}

fn is_test_name(name: &str, subject: &str) -> bool {
    name.strip_prefix(subject)
        .and_then(|rest| rest.strip_prefix('_'))
        .and_then(|rest| {
            rest.strip_prefix("should_")
                .or_else(|| rest.split_once("_should_").map(|(_, text)| text))
        })
        .is_some_and(|text| !text.is_empty())
}

fn to_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut snake = String::with_capacity(name.len() + 4);

    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1);
            let word_start = prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(char::is_uppercase) && next.is_some_and(|n| n.is_lowercase()));

            if word_start && !snake.ends_with('_') {
                snake.push('_');
            }

            snake.extend(c.to_lowercase());
        } else {
            snake.push(*c);
        }
    }

    snake
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::source_map::parse_source;

    fn issues(source: &str) -> Vec<(usize, IssueKind)> {
        let map = SourceMap {
            files: HashMap::from([(
                PathBuf::from("src/lib.rs"),
                parse_source(source, false).unwrap(),
            )]),
            errors: HashMap::new(),
        };

        check(&map).into_iter().map(|i| (i.line, i.kind)).collect()
    }

    fn comment(text: &str) -> IssueKind {
        IssueKind::Comment {
            text: text.to_string(),
        }
    }

    mod check {
        use super::*;

        #[test]
        fn check_valid_structure_should_report_nothing() {
            let source = "\
struct Cache;
impl Cache {
    fn get(&self) {}
}
impl std::fmt::Display for Cache {
    fn fmt(&self) {}
}
impl fmt::Write for Cache {
    fn write_str(&mut self) {}
}
trait Fs {
    fn read_to_string(&self) {}
}
fn normalize() {}
struct Foo;
impl Foo {
    fn bar() {}
}
fn foo() {}
mod outer {
    struct Inner;
    #[cfg(test)]
    mod tests {
        mod inner {
            #[test]
            fn inner_should_work() {}
        }
    }
}
#[cfg(test)]
mod tests {
    mod fixtures {
        pub fn data() {}
    }
    mod cache {
        #[test]
        fn cache_should_evict_oldest() {}
        mod fixtures {}
        mod get {
            #[test]
            fn get_should_return_value() {}
            #[test]
            fn get_missing_should_return_none() {}
        }
        mod std_fmt_display {
            mod fmt {
                #[test]
                fn fmt_should_show_size() {
                    // GIVEN a cache
                    // AND an entry
                    // WHEN formatted
                    // THEN the size shows
                    // AND nothing else
                    // WHY: the size is not part of the code
                }
            }
        }
        mod fmt_write {
            mod write_str {
                #[test]
                fn write_str_should_append() {}
            }
        }
    }
    mod fs {
        mod read_to_string {
            #[test]
            fn read_to_string_should_read() {}
        }
    }
    mod normalize {
        #[test]
        fn normalize_should_strip_dots() {}
    }
    mod foo {
        mod bar {
            #[test]
            fn bar_should_work() {}
        }
    }
}
";
            assert_eq!(issues(source), vec![]);
        }

        #[test]
        fn check_test_name_should_require_subject_then_should() {
            for (test, valid) in [
                ("foo_should_work", true),
                ("foo_missing_arg_should_fail", true),
                ("bar_should_work", false),
                ("foobar_should_work", false),
                ("foo_works", false),
                ("foo_should_", false),
                ("foo_should", false),
                ("should_work", false),
            ] {
                let source = format!(
                    "fn foo() {{}}\n#[cfg(test)]\nmod tests {{ mod foo {{\n#[test]\nfn {test}() {{}}\n}} }}"
                );
                let expected = if valid {
                    vec![]
                } else {
                    vec![(
                        5,
                        IssueKind::TestName {
                            test: test.to_string(),
                            subject: "foo".to_string(),
                        },
                    )]
                };

                assert_eq!(issues(&source), expected, "{test}");
            }
        }

        #[test]
        fn check_tests_outside_subject_modules_should_be_misplaced() {
            let source = "\
struct S;
impl Iterator for S {
    fn next(&mut self) {}
    fn size_hint(&self) {}
}
#[test]
fn loose() {}
#[cfg(test)]
mod tests {
    #[test]
    fn root_should_work() {}
    mod s {
        mod iterator {
            #[test]
            fn iterator_should_work() {}
            #[test]
            fn next_should_advance() {}
            mod size_hint { #[test] fn size_hint_should_work() {} }
            mod missing { #[test] fn missing_should_work() {} }
            mod fixtures {}
        }
    }
}
";
            let misplaced = |test: &str| IssueKind::TestPlacement {
                test: test.to_string(),
            };

            assert_eq!(
                issues(source),
                vec![
                    (7, misplaced("loose")),
                    (11, misplaced("root_should_work")),
                    (15, misplaced("iterator_should_work")),
                    (
                        17,
                        IssueKind::TestModule {
                            test: "next_should_advance".to_string(),
                            module: "s::iterator::next".to_string(),
                        }
                    ),
                    (
                        19,
                        IssueKind::UnknownModule {
                            module: "missing".to_string(),
                        }
                    ),
                ]
            );
        }

        #[test]
        fn check_modules_not_matching_code_should_be_unknown() {
            let source = "\
struct S;
impl S {
    fn get() {}
}
impl Deref for S {
    type Target = u8;
    fn deref(&self) {}
}
fn f() {}
#[cfg(test)]
mod tests {
    mod missing { #[test] fn missing_should_work() {} }
    mod s {
        mod set { mod deeper { #[test] fn deeper_should_work() {} } mod fixtures {} }
        mod target { #[test] fn target_wrong() { /* note */ } }
        mod get {
            mod nested { #[test] fn nested_should_work() {} }
        }
        mod deref {
            mod other { #[test] fn other_should_work() {} }
            mod fixtures {}
        }
    }
    mod f {
        mod nested { #[test] fn nested_should_work() {} }
    }
    mod helper {}
}
";
            let unknown = |module: &str| IssueKind::UnknownModule {
                module: module.to_string(),
            };

            assert_eq!(
                issues(source),
                vec![
                    (12, unknown("missing")),
                    (14, unknown("set")),
                    (15, unknown("target")),
                    (
                        15,
                        IssueKind::TestName {
                            test: "target_wrong".to_string(),
                            subject: "target".to_string(),
                        },
                    ),
                    (15, comment("note")),
                    (17, unknown("nested")),
                    (20, unknown("other")),
                    (25, unknown("nested")),
                ]
            );
        }

        #[test]
        fn check_comments_should_warn_unless_step_or_why() {
            for (comment_text, warn) in [
                ("GIVEN a value", false),
                ("GIVEN: a value", false),
                ("WHY: POSIX rejects append+truncate", false),
                ("GIVEN a value that is\n// long enough to wrap", false),
                ("note", true),
                ("Given a value", true),
                ("GIVENS", true),
                ("EXPLAIN: old keyword", true),
                ("TODO tidy\n// GIVEN a\n// AND b", true),
                ("WHY: POSIX rejects append\n// AND truncate together", false),
                (
                    "WHY: POSIX rejects\n// opening with append\n// AND truncate",
                    false,
                ),
            ] {
                let source = format!(
                    "fn f() {{}}\n#[cfg(test)]\nmod tests {{ mod f {{\n#[test]\nfn f_should_work() {{\n// {comment_text}\n}}\n}} }}"
                );
                let expected = if warn {
                    vec![(6, comment(&comment_text.replace("\n// ", "\n")))]
                } else {
                    vec![]
                };

                assert_eq!(issues(&source), expected, "{comment_text}");
            }
        }

        #[test]
        fn check_comments_should_warn_when_out_of_order() {
            for (steps, out_of_order) in [
                (&["GIVEN", "THEN", "WHEN", "THEN", "AND"][..], None),
                (&["WHEN", "AND", "THEN"], None),
                (&["AND", "GIVEN"], Some("AND")),
                (&["GIVEN", "THEN", "AND"], None),
                (&["WHEN", "", "GIVEN"], Some("GIVEN")),
                (&["GIVEN", "WHEN", "GIVEN"], Some("GIVEN")),
            ] {
                let body = steps
                    .iter()
                    .map(|s| format!("// {s}\n"))
                    .collect::<String>();
                let source = format!(
                    "fn f() {{}}\n#[cfg(test)]\nmod tests {{ mod f {{\n#[test]\nfn f_should_work() {{\n{body}}}\n}} }}"
                );
                let expected: Vec<(usize, IssueKind)> = out_of_order
                    .iter()
                    .map(|text| {
                        let line = 6 + steps.iter().rposition(|s| s == text).unwrap();
                        (
                            line,
                            IssueKind::CommentOrder {
                                text: text.to_string(),
                            },
                        )
                    })
                    .collect();

                assert_eq!(issues(&source), expected, "{steps:?}");
            }
        }

        #[test]
        fn check_test_file_should_name_items_of_its_crate() {
            let file = |path: &str, krate: &str, source: &str| {
                let mut types = parse_source(source, false).unwrap();
                types.crate_dir = Some(PathBuf::from(krate));
                (PathBuf::from(path), types)
            };
            let tests = "\
mod cache {
    mod get {
        #[test]
        fn get_should_work() {}
    }
    mod insert {
        #[test]
        fn insert_should_work() {}
    }
}
";
            let map = SourceMap {
                files: HashMap::from([
                    file(
                        "a/src/cache.rs",
                        "a",
                        "struct Cache;\nimpl Cache { fn get() {} }",
                    ),
                    file("a/src/ext.rs", "a", "impl Cache { fn insert() {} }"),
                    file("a/src/cache_tests.rs", "a", tests),
                    file("b/tests/cache.rs", "b", tests),
                    file(
                        "a/src/inner.rs",
                        "a",
                        "mod inner {\n    struct Inner;\n}\n#[cfg(test)]\nmod tests {\n    mod cache {\n        #[test]\n        fn cache_should_work() {}\n    }\n}",
                    ),
                    file(
                        "a/src/other.rs",
                        "a",
                        "fn other() {}\n#[cfg(test)]\nmod tests {\n    mod cache {\n        #[test]\n        fn cache_should_work() {}\n    }\n}",
                    ),
                ]),
                errors: HashMap::new(),
            };
            let unknown = |file: &str, line: usize| Issue {
                file: PathBuf::from(file),
                line,
                kind: IssueKind::UnknownModule {
                    module: "cache".to_string(),
                },
            };

            assert_eq!(
                check(&map),
                vec![
                    unknown("a/src/inner.rs", 6),
                    unknown("a/src/other.rs", 4),
                    unknown("b/tests/cache.rs", 1),
                ]
            );
        }

        #[test]
        fn check_should_report_unparsable_files() {
            let map = SourceMap {
                files: HashMap::new(),
                errors: HashMap::from([(
                    PathBuf::from("src/lib.rs"),
                    "1:4: expected identifier".to_string(),
                )]),
            };

            assert_eq!(
                check(&map),
                vec![Issue {
                    file: PathBuf::from("src/lib.rs"),
                    line: 0,
                    kind: IssueKind::Unparsable {
                        error: "1:4: expected identifier".to_string(),
                    },
                }]
            );
        }

        #[test]
        fn check_trait_impl_without_methods_should_accept_direct_tests() {
            let source = "\
struct E;
impl std::error::Error for E {}
#[cfg(test)]
mod tests {
    mod e {
        mod std_error_error {
            #[test]
            fn std_error_error_should_be_implemented() {}
        }
    }
}
";
            assert_eq!(issues(source), vec![]);
        }

        #[test]
        fn check_blanket_or_foreign_type_impl_should_place_tests_under_trait() {
            for code in [
                "trait Ext {\n    fn ext(&self);\n    fn twice(&self) {}\n}\nimpl<T: Display> Ext for T {\n    fn ext(&self) {}\n}\n",
                "impl<T: Display> other::Ext for T {\n    fn ext(&self) {}\n    fn twice(&self) {}\n}\n",
                "impl Ext for Vec<u8> {\n    fn ext(&self) {}\n    fn twice(&self) {}\n}\n",
            ] {
                let source = format!(
                    "{code}#[cfg(test)]\nmod tests {{ mod ext {{\n#[test]\nfn ext_should_extend_everything() {{}}\nmod ext {{ #[test] fn ext_should_work() {{}} }}\nmod twice {{ #[test] fn twice_should_work() {{}} }}\n}} }}"
                );

                assert_eq!(issues(&source), vec![], "{code}");
            }
        }

        #[test]
        fn check_single_method_trait_impl_should_accept_direct_tests() {
            let source = "\
struct S;
impl Drop for S {
    fn drop(&mut self) {}
}
impl std::fmt::Display for S {
    fn fmt(&self) {}
}
#[cfg(test)]
mod tests {
    mod s {
        mod drop {
            #[test]
            fn drop_should_release() {}
            mod drop {
                #[test]
                fn drop_should_release_too() {}
            }
        }
        mod std_fmt_display {
            #[test]
            fn std_fmt_display_should_show() {}
            #[test]
            fn fmt_should_show() {}
            #[test]
            fn show_should_work() {}
        }
    }
}
";
            assert_eq!(
                issues(source),
                vec![(
                    25,
                    IssueKind::TestName {
                        test: "show_should_work".to_string(),
                        subject: "std_fmt_display".to_string(),
                    }
                )]
            );
        }

        #[test]
        fn check_same_named_method_and_trait_should_resolve_by_children() {
            let code = "struct S;\nimpl S {\n    fn write() {}\n}\nimpl Write for S {\n    fn write(&mut self) {}\n}\n";

            for tests in [
                "mod s { mod write { #[test] fn write_should_inherit() {} } }",
                "mod s { mod write { mod write { #[test] fn write_should_implement() {} } } }",
            ] {
                let source = format!("{code}#[cfg(test)]\nmod tests {{ {tests} }}");

                assert_eq!(issues(&source), vec![], "{tests}");
            }
        }

        #[test]
        fn check_cross_method_test_after_method_modules_should_be_out_of_order() {
            let source = "\
struct S;
impl S {
    fn get() {}
}
#[cfg(test)]
mod tests {
    mod s {
        mod get {
            #[test]
            fn get_should_work() {}
        }
        #[test]
        fn s_should_work() {}
    }
}
";
            assert_eq!(
                issues(source),
                vec![(
                    13,
                    IssueKind::TestOrder {
                        test: "s_should_work".to_string(),
                    }
                )]
            );
        }

        #[test]
        fn check_test_after_unknown_module_should_not_be_out_of_order() {
            let source = "\
struct S;
impl S { fn get() {} }
#[cfg(test)]
mod tests {
    mod s {
        mod typo { #[test] fn typo_should_work() {} }
        #[test]
        fn s_should_work() {}
        mod get {
            mod nested { #[test] fn nested_should_work() {} }
            #[test]
            fn get_should_work() {}
        }
    }
}
";
            let unknown = |module: &str| IssueKind::UnknownModule {
                module: module.to_string(),
            };

            assert_eq!(
                issues(source),
                vec![(6, unknown("typo")), (10, unknown("nested"))]
            );
        }

        #[test]
        fn check_macro_should_be_subject_like_function() {
            let source = "\
macro_rules! env_error { () => {} }
#[cfg(test)]
mod tests {
    mod env_error {
        #[test]
        fn env_error_should_format() {}
    }
}
";
            assert_eq!(issues(source), vec![]);
        }

        #[test]
        fn check_method_test_in_type_module_should_name_method_module() {
            let source = "\
enum FileType { Dir }
impl FileType {
    fn is_dir(&self) -> bool { true }
    fn is_dir_or_file(&self) -> bool { true }
}
#[cfg(test)]
mod tests {
    mod file_type {
        #[test]
        fn is_dir_should_return_true() {
            // note
        }
        #[test]
        fn is_dir_or_file_should_return_true() {}
        #[test]
        fn other_should_work() {}
    }
}
";
            let in_module = |test: &str, module: &str| IssueKind::TestModule {
                test: test.to_string(),
                module: module.to_string(),
            };

            assert_eq!(
                issues(source),
                vec![
                    (
                        10,
                        in_module("is_dir_should_return_true", "file_type::is_dir")
                    ),
                    (11, comment("note")),
                    (
                        14,
                        in_module(
                            "is_dir_or_file_should_return_true",
                            "file_type::is_dir_or_file"
                        )
                    ),
                    (
                        16,
                        IssueKind::TestName {
                            test: "other_should_work".to_string(),
                            subject: "file_type".to_string(),
                        }
                    ),
                ]
            );
        }

        #[test]
        fn check_trait_method_module_under_type_should_name_its_path() {
            let source = "\
trait Env {
    fn args(&self);
    fn var_bool(&self) {}
}
struct EnvImpl;
impl Env for EnvImpl {
    fn args(&self) {}
}
impl std::fmt::Display for EnvImpl {
    fn fmt(&self) {}
}
#[cfg(test)]
mod tests {
    mod env_impl {
        mod args { #[test] fn args_wrong() {} }
        mod var_bool { #[test] fn var_bool_should_parse() {} }
        mod fmt { #[test] fn fmt_should_display() {} }
        mod missing { #[test] fn missing_should_work() {} }
    }
}
";
            let path = |module: &str, path: &str, direct: bool| IssueKind::TraitMethodModule {
                module: module.to_string(),
                path: path.to_string(),
                direct,
            };

            assert_eq!(
                issues(source),
                vec![
                    (15, path("args", "env_impl::env::args", false)),
                    (
                        15,
                        IssueKind::TestName {
                            test: "args_wrong".to_string(),
                            subject: "args".to_string(),
                        }
                    ),
                    (16, path("var_bool", "env_impl::env::var_bool", false)),
                    (17, path("fmt", "env_impl::std_fmt_display::fmt", true)),
                    (
                        18,
                        IssueKind::UnknownModule {
                            module: "missing".to_string(),
                        }
                    ),
                ]
            );
        }

        #[test]
        fn check_trait_impl_should_include_default_methods_of_trait_in_other_file() {
            let fs = "pub trait Fs {\n    fn open_read(&self);\n    fn open_write(&self) {}\n}\n";
            let test_fs = |tests: &str| {
                format!(
                    "struct TestFs;\nimpl Fs for TestFs {{\n    fn open_read(&self) {{}}\n}}\n#[cfg(test)]\nmod tests {{ mod test_fs {{ {tests} }} }}\n"
                )
            };
            let issues = |files: &[(&str, &str)]| {
                let map = SourceMap {
                    files: files
                        .iter()
                        .map(|(file, source)| {
                            (PathBuf::from(file), parse_source(source, false).unwrap())
                        })
                        .collect(),
                    errors: HashMap::new(),
                };

                check(&map).into_iter().map(|i| i.kind).collect::<Vec<_>>()
            };
            let nested = test_fs(
                "mod fs { mod open_read { #[test] fn open_read_should_read() {} } mod open_write { #[test] fn open_write_should_create() {} } }",
            );
            let flat = test_fs("mod open_write { #[test] fn open_write_should_create() {} }");

            assert_eq!(
                issues(&[("src/fs.rs", fs), ("src/test_fs.rs", &nested)]),
                vec![]
            );
            assert_eq!(
                issues(&[("src/fs.rs", fs), ("src/test_fs.rs", &flat)]),
                vec![IssueKind::TraitMethodModule {
                    module: "open_write".to_string(),
                    path: "test_fs::fs::open_write".to_string(),
                    direct: false,
                }]
            );
            assert_eq!(
                issues(&[("a/fs.rs", fs), ("b/fs.rs", fs), ("src/test_fs.rs", &flat)]),
                vec![IssueKind::UnknownModule {
                    module: "open_write".to_string(),
                }]
            );
        }

        #[test]
        fn check_misplaced_test_should_still_check_comments() {
            let source = "#[cfg(test)]\nmod tests {\n#[test]\nfn t() {\n// note\n}\n}";

            assert_eq!(
                issues(source),
                vec![
                    (
                        4,
                        IssueKind::TestPlacement {
                            test: "t".to_string(),
                        }
                    ),
                    (5, comment("note")),
                ]
            );
        }

        #[test]
        fn check_should_sort_issues_by_file_and_line() {
            let source = "#[test]\nfn b() {}\n#[test]\nfn a() {}";
            let map = SourceMap {
                files: HashMap::from([
                    (
                        PathBuf::from("src/b.rs"),
                        parse_source(source, false).unwrap(),
                    ),
                    (
                        PathBuf::from("src/a.rs"),
                        parse_source(source, false).unwrap(),
                    ),
                ]),
                errors: HashMap::new(),
            };

            assert_eq!(
                check(&map)
                    .into_iter()
                    .map(|i| (i.file, i.line))
                    .collect::<Vec<_>>(),
                vec![
                    (PathBuf::from("src/a.rs"), 2),
                    (PathBuf::from("src/a.rs"), 4),
                    (PathBuf::from("src/b.rs"), 2),
                    (PathBuf::from("src/b.rs"), 4),
                ]
            );
        }
    }

    mod issue_kind {
        use super::*;

        mod std_fmt_display {
            use super::*;

            mod fmt {
                use super::*;

                #[test]
                fn fmt_should_describe_issue_and_rule() {
                    let s = |text: &str| text.to_string();

                    for (kind, expected) in [
                        (
                            IssueKind::Unparsable {
                                error: s("2:4: expected identifier"),
                            },
                            "cannot parse the file: 2:4: expected identifier",
                        ),
                        (
                            IssueKind::TestName {
                                test: s("get_works"),
                                subject: s("get"),
                            },
                            "test `get_works` should be named `get_should_<behaviour>` or `get_<args>_should_<behaviour>`",
                        ),
                        (
                            IssueKind::TestPlacement { test: s("t") },
                            "test `t` should be in the module of its type, trait, function or method",
                        ),
                        (
                            IssueKind::TestModule {
                                test: s("is_dir_should_work"),
                                module: s("file_type::is_dir"),
                            },
                            "test `is_dir_should_work` should be in module `file_type::is_dir`",
                        ),
                        (
                            IssueKind::TraitMethodModule {
                                module: s("args"),
                                path: s("env_impl::env::args"),
                                direct: false,
                            },
                            "module `args` tests a trait method and should be `env_impl::env::args`",
                        ),
                        (
                            IssueKind::TraitMethodModule {
                                module: s("fmt"),
                                path: s("error::std_fmt_display::fmt"),
                                direct: true,
                            },
                            "module `fmt` tests a trait method and should be `error::std_fmt_display` or `error::std_fmt_display::fmt`",
                        ),
                        (
                            IssueKind::TestOrder {
                                test: s("s_should_work"),
                            },
                            "cross-method test `s_should_work` should come before the method modules",
                        ),
                        (
                            IssueKind::UnknownModule {
                                module: s("missing"),
                            },
                            "module `missing` does not match any type, trait, function or method",
                        ),
                        (
                            IssueKind::Comment {
                                text: s("first\nsecond"),
                            },
                            "comment \"first\" should be deleted, start with WHY or GIVEN, WHEN, THEN, AND",
                        ),
                        (
                            IssueKind::CommentOrder {
                                text: s("AND more"),
                            },
                            "step \"AND more\" is out of order: AND cannot come first and GIVEN cannot follow WHEN or THEN",
                        ),
                    ] {
                        assert_eq!(kind.to_string(), expected);
                    }
                }
            }
        }
    }

    mod to_snake_case {
        use super::*;

        #[test]
        fn to_snake_case_should_convert_type_names() {
            for (input, expected) in [
                ("Cache", "cache"),
                ("FsImpl", "fs_impl"),
                ("HTTPServer", "http_server"),
                ("IOError", "io_error"),
                ("Utf8Error", "utf8_error"),
                ("ABC", "abc"),
                ("already_snake", "already_snake"),
                ("Mixed_Case", "mixed_case"),
            ] {
                assert_eq!(to_snake_case(input), expected, "{input}");
            }
        }
    }
}
