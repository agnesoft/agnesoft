use std::collections::HashMap;
use std::path::PathBuf;

use agplatform::Fs;
use agplatform::Platform;
use syn::Attribute;
use syn::ImplItem;
use syn::Item;
use syn::ItemFn;
use syn::ItemImpl;
use syn::ItemMod;
use syn::ItemTrait;
use syn::Meta;
use syn::Token;
use syn::TraitItem;
use syn::Type;
use syn::TypeParamBound;
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceTypeKind {
    Type,
    Trait,
    Function,
    Method,
    Module(ModuleKind),
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModuleKind {
    Source,
    Test,
    TestHelper,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceType {
    pub(crate) kind: SourceTypeKind,
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) types: Vec<SourceType>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SourceTypes {
    pub(crate) types: HashMap<String, Vec<SourceType>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SourceMap {
    pub(crate) files: HashMap<PathBuf, SourceTypes>,
    pub(crate) errors: HashMap<PathBuf, String>,
}

#[allow(dead_code)]
pub(crate) fn source_map(p: &impl Platform, sources: &[PathBuf]) -> SourceMap {
    let mut map = SourceMap::default();

    for source in sources {
        match p
            .fs()
            .read_to_string(source)
            .map_err(|e| e.to_string())
            .and_then(|content| parse_source(&content))
        {
            Ok(types) => {
                map.files.insert(source.clone(), types);
            }
            Err(e) => {
                map.errors.insert(source.clone(), e);
            }
        }
    }

    map
}

pub(crate) fn parse_source(content: &str) -> Result<SourceTypes, String> {
    let result = syn::parse_file(content)
        .map(|file| parse_items(&file.items, is_cfg_test(&file.attrs)))
        .map_err(|e| parse_error(content, &e));

    // With span-locations every parsed source is otherwise kept in a
    // thread local store until the thread exits.
    proc_macro2::extra::invalidate_current_thread_spans();

    let mut types = result?;
    sort(&mut types);

    let mut source_types = SourceTypes::default();

    for source_type in types {
        source_types
            .types
            .entry(source_type.name.clone())
            .or_default()
            .push(source_type);
    }

    Ok(source_types)
}

fn parse_error(content: &str, e: &syn::Error) -> String {
    let span = e.span();

    // Errors at the end of input carry an empty call site span.
    let (line, column) = if span.byte_range().is_empty() {
        let last_line = content.rsplit('\n').next().unwrap_or_default();
        (content.matches('\n').count() + 1, last_line.chars().count())
    } else {
        (span.start().line, span.start().column)
    };

    format!("{line}:{}: {e}", column + 1)
}

fn parse_items(items: &[Item], in_test: bool) -> Vec<SourceType> {
    let mut types = Vec::new();
    let mut impls = Vec::new();

    for item in items {
        let source_type = match item {
            Item::Fn(i) if is_test_fn(i, in_test) => {
                SourceType::new(SourceTypeKind::Test, &i.sig.ident)
            }
            Item::Mod(
                i @ ItemMod {
                    content: Some((_, items)),
                    ..
                },
            ) => module_type(&i.ident, items, in_test || is_cfg_test(&i.attrs)),
            _ if in_test || is_cfg_test(item_attrs(item)) => continue,
            Item::Enum(i) => SourceType::new(SourceTypeKind::Type, &i.ident),
            Item::Struct(i) => SourceType::new(SourceTypeKind::Type, &i.ident),
            Item::Union(i) => SourceType::new(SourceTypeKind::Type, &i.ident),
            Item::Trait(i) => trait_type(i),
            Item::Fn(i) => SourceType::new(SourceTypeKind::Function, &i.sig.ident),
            Item::Impl(i) => {
                impls.push(i);
                continue;
            }
            _ => continue,
        };

        types.push(source_type);
    }

    // Trait impls are named and grouped by the target and the trait path as
    // written (without generics) so that e.g. `From<A>` and `From<B>` share a
    // module but `fmt::Write` and `io::Write` do not.
    let mut trait_impls: Vec<(ImplTarget, SourceType)> = Vec::new();

    for i in impls {
        let items: Vec<SourceType> = impl_types(i).collect();

        let Some(target) = impl_target(i) else {
            continue;
        };

        match target.trait_path {
            Some(path) => {
                let trait_name = path_string(path);

                match trait_impls.iter_mut().find(|(t, trait_type)| {
                    t.ident == target.ident && trait_type.name == trait_name
                }) {
                    Some((_, trait_type)) => trait_type.types.extend(items),
                    None => {
                        let mut trait_type =
                            SourceType::new(SourceTypeKind::Trait, last_segment(path));
                        trait_type.name = trait_name;
                        trait_type.types = items;
                        trait_impls.push((target, trait_type));
                    }
                }
            }
            None => target.find_or_insert(&mut types).types.extend(items),
        }
    }

    for (target, trait_type) in trait_impls {
        target.find_or_insert(&mut types).types.push(trait_type);
    }

    types
}

fn trait_type(t: &ItemTrait) -> SourceType {
    let mut source_type = SourceType::new(SourceTypeKind::Trait, &t.ident);

    source_type
        .types
        .extend(t.items.iter().filter_map(|item| match item {
            TraitItem::Fn(f) if f.default.is_some() && !is_cfg_test(&f.attrs) => {
                Some(SourceType::new(SourceTypeKind::Method, &f.sig.ident))
            }
            TraitItem::Type(t) if t.default.is_some() && !is_cfg_test(&t.attrs) => {
                Some(SourceType::new(SourceTypeKind::Type, &t.ident))
            }
            _ => None,
        }));

    source_type
}

fn impl_types(i: &ItemImpl) -> impl Iterator<Item = SourceType> + '_ {
    i.items.iter().filter_map(|item| match item {
        ImplItem::Fn(f) if !is_cfg_test(&f.attrs) => {
            Some(SourceType::new(SourceTypeKind::Method, &f.sig.ident))
        }
        ImplItem::Type(t) if !is_cfg_test(&t.attrs) => {
            Some(SourceType::new(SourceTypeKind::Type, &t.ident))
        }
        _ => None,
    })
}

fn module_type(ident: &syn::Ident, items: &[Item], in_test: bool) -> SourceType {
    let in_test = in_test
        || items
            .iter()
            .any(|item| matches!(item, Item::Fn(f) if is_test_fn(f, false)));
    let types = parse_items(items, in_test);
    let kind = if !in_test {
        ModuleKind::Source
    } else if types.iter().any(SourceType::has_tests) {
        ModuleKind::Test
    } else {
        ModuleKind::TestHelper
    };

    let mut source_type = SourceType::new(SourceTypeKind::Module(kind), ident);
    source_type.types = types;
    source_type
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn sort(types: &mut [SourceType]) {
    types.sort_by_key(|t| t.line);

    for source_type in types {
        sort(&mut source_type.types);
    }
}

impl SourceType {
    fn has_tests(&self) -> bool {
        matches!(
            self.kind,
            SourceTypeKind::Test | SourceTypeKind::Module(ModuleKind::Test)
        )
    }

    fn new(kind: SourceTypeKind, ident: &syn::Ident) -> Self {
        Self {
            kind,
            name: name(ident),
            line: line(ident),
            types: Vec::new(),
        }
    }
}

/// The type or trait an impl's items belong under.
struct ImplTarget<'a> {
    ident: &'a syn::Ident,
    kind: SourceTypeKind,
    /// The implemented trait when its items nest under the target.
    trait_path: Option<&'a syn::Path>,
}

impl ImplTarget<'_> {
    /// Impls of types defined elsewhere (e.g. in another file) have no
    /// definition so the first impl stands in for it.
    fn find_or_insert<'t>(&self, types: &'t mut Vec<SourceType>) -> &'t mut SourceType {
        let name = name(self.ident);
        let index = match types.iter().position(|t| {
            matches!(t.kind, SourceTypeKind::Type | SourceTypeKind::Trait) && t.name == name
        }) {
            Some(index) => index,
            None => {
                types.push(SourceType::new(self.kind.clone(), self.ident));
                types.len() - 1
            }
        };

        &mut types[index]
    }
}

/// Impls for a named type or `dyn Trait` belong under it. Blanket impls
/// (`impl<T: Bound> Trait for T`) and trait impls for types without a name
/// (slices, tuples etc.) belong under the trait itself.
fn impl_target(i: &ItemImpl) -> Option<ImplTarget<'_>> {
    let trait_path = i.trait_.as_ref().map(|(path, _)| path);

    match self_type(&i.self_ty) {
        Some((ident, kind)) if !i.generics.type_params().any(|p| p.ident == *ident) => {
            Some(ImplTarget {
                ident,
                kind,
                trait_path,
            })
        }
        _ => trait_path.map(|path| ImplTarget {
            ident: last_segment(path),
            kind: SourceTypeKind::Trait,
            trait_path: None,
        }),
    }
}

fn self_type(ty: &Type) -> Option<(&syn::Ident, SourceTypeKind)> {
    match ty {
        Type::Path(p) if p.qself.is_none() => p
            .path
            .segments
            .last()
            .map(|s| (&s.ident, SourceTypeKind::Type)),
        Type::TraitObject(t) => t.bounds.iter().find_map(|b| match b {
            TypeParamBound::Trait(t) => Some((last_segment(&t.path), SourceTypeKind::Trait)),
            _ => None,
        }),
        Type::Reference(r) => self_type(&r.elem),
        Type::Paren(p) => self_type(&p.elem),
        _ => None,
    }
}

fn last_segment(path: &syn::Path) -> &syn::Ident {
    &path
        .segments
        .last()
        .expect("path should have at least one segment")
        .ident
}

fn path_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| name(&s.ident))
        .collect::<Vec<_>>()
        .join("::")
}

fn is_test_fn(f: &ItemFn, in_test: bool) -> bool {
    let in_test = in_test || is_cfg_test(&f.attrs);

    f.attrs.iter().any(|a| {
        let path = a.path();
        path.is_ident("test")
            || (in_test && path.segments.last().is_some_and(|s| s.ident == "test"))
    })
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().is_ident("cfg") && a.parse_args::<Meta>().is_ok_and(|m| implies_test(&m)))
}

/// Whether the cfg predicate can only hold in test builds, e.g. `test` or
/// `all(test, unix)` but not `any(test, feature = "x")` or `not(test)`.
fn implies_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test"),
        Meta::List(l) if l.path.is_ident("all") || l.path.is_ident("any") => l
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|predicates| {
                if l.path.is_ident("all") {
                    predicates.iter().any(implies_test)
                } else {
                    predicates.iter().all(implies_test)
                }
            }),
        _ => false,
    }
}

fn name(ident: &syn::Ident) -> String {
    ident.unraw().to_string()
}

fn line(ident: &syn::Ident) -> usize {
    ident.span().start().line
}

#[cfg(test)]
mod tests {
    use super::*;
    use agplatform::test_platform::test_platform;

    fn ty(kind: SourceTypeKind, name: &str, line: usize, types: Vec<SourceType>) -> SourceType {
        SourceType {
            kind,
            name: name.to_string(),
            line,
            types,
        }
    }

    const MODULE: SourceTypeKind = SourceTypeKind::Module(ModuleKind::Source);
    const TEST_MODULE: SourceTypeKind = SourceTypeKind::Module(ModuleKind::Test);
    const HELPER_MODULE: SourceTypeKind = SourceTypeKind::Module(ModuleKind::TestHelper);

    fn leaf(kind: SourceTypeKind, name: &str, line: usize) -> SourceType {
        ty(kind, name, line, Vec::new())
    }

    fn types(types: Vec<SourceType>) -> SourceTypes {
        let mut source_types = SourceTypes::default();

        for t in types {
            source_types
                .types
                .entry(t.name.clone())
                .or_default()
                .push(t);
        }

        source_types
    }

    mod source_map {
        use super::*;

        #[test]
        fn source_map_should_map_each_source() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            platform.fs_mut().write("src/lib.rs", b"struct S;").unwrap();

            let map = source_map(
                &platform,
                &[PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")],
            );

            assert_eq!(map.errors, HashMap::new());
            assert_eq!(
                map.files,
                HashMap::from([
                    (
                        PathBuf::from("src/main.rs"),
                        types(vec![leaf(SourceTypeKind::Function, "main", 1)])
                    ),
                    (
                        PathBuf::from("src/lib.rs"),
                        types(vec![leaf(SourceTypeKind::Type, "S", 1)])
                    ),
                ])
            );
        }

        #[test]
        fn source_map_should_record_unreadable_and_unparsable_sources() {
            let mut platform = test_platform();
            platform.fs_mut().write("invalid.rs", b"fn () {}").unwrap();

            let map = source_map(
                &platform,
                &[PathBuf::from("missing.rs"), PathBuf::from("invalid.rs")],
            );

            assert_eq!(map.files, HashMap::new());
            assert_eq!(map.errors.len(), 2);
            assert_eq!(
                map.errors[&PathBuf::from("invalid.rs")],
                "1:4: expected identifier"
            );
            assert!(map.errors.contains_key(&PathBuf::from("missing.rs")));
        }
    }

    mod parse_source {
        use super::*;

        #[test]
        fn parse_source_should_map_types_traits_and_functions() {
            let source = "\
enum MyEnum { A }
struct MyStruct;
union MyUnion { a: u32 }
trait MyTrait {}
fn my_fn() {}
fn r#type() {}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Type, "MyEnum", 1),
                    leaf(SourceTypeKind::Type, "MyStruct", 2),
                    leaf(SourceTypeKind::Type, "MyUnion", 3),
                    leaf(SourceTypeKind::Trait, "MyTrait", 4),
                    leaf(SourceTypeKind::Function, "my_fn", 5),
                    leaf(SourceTypeKind::Function, "type", 6),
                ])
            );
        }

        #[test]
        fn parse_source_should_nest_impl_items_under_type() {
            let source = "\
impl Cache {
    fn get(&self) {}
}
struct Cache;
impl Deref for Cache {
    type Target = u32;
    fn deref(&self) -> &u32 { &0 }
}
impl<T> Wrapper<T> {
    fn new() {}
}
impl Display for &Cache {
    fn fmt(&self) {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    ty(
                        SourceTypeKind::Type,
                        "Cache",
                        4,
                        vec![
                            leaf(SourceTypeKind::Method, "get", 2),
                            ty(
                                SourceTypeKind::Trait,
                                "Deref",
                                5,
                                vec![
                                    leaf(SourceTypeKind::Type, "Target", 6),
                                    leaf(SourceTypeKind::Method, "deref", 7),
                                ]
                            ),
                            ty(
                                SourceTypeKind::Trait,
                                "Display",
                                12,
                                vec![leaf(SourceTypeKind::Method, "fmt", 13)]
                            ),
                        ]
                    ),
                    ty(
                        SourceTypeKind::Type,
                        "Wrapper",
                        9,
                        vec![leaf(SourceTypeKind::Method, "new", 10)]
                    ),
                ])
            );
        }

        #[test]
        fn parse_source_should_nest_trait_impl_items_under_trait() {
            let source = "\
struct S;
impl S {
    fn fmt(&self) {}
}
impl Display for S {
    fn fmt(&self) {}
}
impl From<A> for S {
    fn from(a: A) -> Self { S }
}
impl From<B> for S {
    fn from(b: B) -> Self { S }
}
impl fmt::Write for S {
    fn write_str(&mut self) {}
}
impl io::Write for S {
    fn write(&mut self) {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    SourceTypeKind::Type,
                    "S",
                    1,
                    vec![
                        leaf(SourceTypeKind::Method, "fmt", 3),
                        ty(
                            SourceTypeKind::Trait,
                            "Display",
                            5,
                            vec![leaf(SourceTypeKind::Method, "fmt", 6)]
                        ),
                        ty(
                            SourceTypeKind::Trait,
                            "From",
                            8,
                            vec![
                                leaf(SourceTypeKind::Method, "from", 9),
                                leaf(SourceTypeKind::Method, "from", 12),
                            ]
                        ),
                        ty(
                            SourceTypeKind::Trait,
                            "fmt::Write",
                            14,
                            vec![leaf(SourceTypeKind::Method, "write_str", 15)]
                        ),
                        ty(
                            SourceTypeKind::Trait,
                            "io::Write",
                            17,
                            vec![leaf(SourceTypeKind::Method, "write", 18)]
                        ),
                    ]
                )])
            );
        }

        #[test]
        fn parse_source_should_nest_default_and_blanket_items_under_trait() {
            let source = "\
trait Fs {
    type In;
    type Out = u8;
    fn read(&self);
    fn read_to_string(&self) {}
}
impl<T: Read> Fs for T {
    type In = u8;
    fn read(&self) {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    SourceTypeKind::Trait,
                    "Fs",
                    1,
                    vec![
                        leaf(SourceTypeKind::Type, "Out", 3),
                        leaf(SourceTypeKind::Method, "read_to_string", 5),
                        leaf(SourceTypeKind::Type, "In", 8),
                        leaf(SourceTypeKind::Method, "read", 9),
                    ]
                )])
            );
        }

        #[test]
        fn parse_source_should_map_blanket_impl_of_external_trait_as_trait() {
            let source = "\
impl<T: Display> MyExt for T {
    fn ext(&self) {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    SourceTypeKind::Trait,
                    "MyExt",
                    1,
                    vec![leaf(SourceTypeKind::Method, "ext", 2)]
                )])
            );
        }

        #[test]
        fn parse_source_should_map_impls_for_unnamed_types_under_trait() {
            let source = "\
impl MyTrait for [u8] {
    fn slice(&self) {}
}
impl MyTrait for (A, B) {
    fn tuple(&self) {}
}
impl MyTrait for <S as T>::Assoc {
    fn assoc(&self) {}
}
impl dyn MyTrait {
    fn object(&self) {}
}
impl Display for dyn 'static + MyTrait + Send {
    fn fmt(&self) {}
}
impl [u8] {
    fn inherent(&self) {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    SourceTypeKind::Trait,
                    "MyTrait",
                    1,
                    vec![
                        leaf(SourceTypeKind::Method, "slice", 2),
                        leaf(SourceTypeKind::Method, "tuple", 5),
                        leaf(SourceTypeKind::Method, "assoc", 8),
                        leaf(SourceTypeKind::Method, "object", 11),
                        ty(
                            SourceTypeKind::Trait,
                            "Display",
                            13,
                            vec![leaf(SourceTypeKind::Method, "fmt", 14)]
                        ),
                    ]
                )])
            );
        }

        #[test]
        fn parse_source_should_map_impls_without_items() {
            let source = "\
impl Error for MyError {}
unsafe impl Send for MyError {}
impl Y {}
impl<T> Marker for T {}
impl Z {
    const C: u32 = 0;
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    ty(
                        SourceTypeKind::Type,
                        "MyError",
                        1,
                        vec![
                            leaf(SourceTypeKind::Trait, "Error", 1),
                            leaf(SourceTypeKind::Trait, "Send", 2),
                        ]
                    ),
                    leaf(SourceTypeKind::Type, "Y", 3),
                    leaf(SourceTypeKind::Trait, "Marker", 4),
                    leaf(SourceTypeKind::Type, "Z", 5),
                ])
            );
        }

        #[test]
        fn parse_source_should_keep_items_of_same_name() {
            let source = "\
fn foo() {}
mod foo {}
struct Foo;
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Function, "foo", 1),
                    leaf(MODULE, "foo", 2),
                    leaf(SourceTypeKind::Type, "Foo", 3),
                ])
            );
        }

        #[test]
        fn parse_source_should_attach_impl_to_type_not_function_of_same_name() {
            let source = "\
fn bar() {}
struct bar;
impl bar {
    fn run() {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Function, "bar", 1),
                    ty(
                        SourceTypeKind::Type,
                        "bar",
                        2,
                        vec![leaf(SourceTypeKind::Method, "run", 4)]
                    ),
                ])
            );
        }

        #[test]
        fn parse_source_should_nest_inline_modules() {
            let source = "\
mod outer {
    struct Inner;
    fn inner() {}
    mod nested {
        fn deep() {}
    }
}
mod empty {}
mod external;
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    ty(
                        MODULE,
                        "outer",
                        1,
                        vec![
                            leaf(SourceTypeKind::Type, "Inner", 2),
                            leaf(SourceTypeKind::Function, "inner", 3),
                            ty(
                                MODULE,
                                "nested",
                                4,
                                vec![leaf(SourceTypeKind::Function, "deep", 5)]
                            ),
                        ]
                    ),
                    leaf(MODULE, "empty", 8),
                ])
            );
        }

        #[test]
        fn parse_source_should_ignore_test_and_unmapped_items() {
            let source = "\
#[cfg(test)]
fn test_helper() {}
#[cfg(test)]
struct TestOnly;
#[cfg(test)]
impl S {
    fn test_impl() {}
}
#[cfg(all(test, unix))]
fn test_unix() {}
#[cfg(any(test, all(test, feature = \"x\")))]
fn test_any() {}
#[cfg(any())]
fn never() {}
#[cfg(all(test,))]
struct TrailingComma;
struct S;
impl S {
    #[cfg(test)]
    fn test_helper() {}
    #[cfg(test)]
    type TestType = u8;
    const C: u32 = 0;
}
trait T {
    #[cfg(test)]
    fn test_default() {}
    #[cfg(test)]
    type TestType = u8;
    const C: u32;
}
const C: u32 = 0;
static ST: u32 = 0;
type Alias = S;
macro_rules! m { () => {} }
use std::path::Path;
extern crate alloc;
extern \"C\" {}
trait Alias = T;
macro m2() {}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Type, "S", 17),
                    leaf(SourceTypeKind::Trait, "T", 25),
                ])
            );
        }

        #[test]
        fn parse_source_should_keep_items_not_exclusive_to_tests() {
            let source = "\
#[cfg(feature = \"x\")]
fn featured() {}
#[cfg(any(test, feature = \"x\"))]
fn test_or_featured() {}
#[cfg(not(test))]
fn not_test() {}
#[cfg(all(unix, feature = \"x\"))]
fn unix_featured() {}
#[cfg_attr(test, derive(Debug))]
struct Derived;
#[cfg]
fn bare() {}
#[cfg(all(1))]
fn invalid() {}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Function, "featured", 2),
                    leaf(SourceTypeKind::Function, "test_or_featured", 4),
                    leaf(SourceTypeKind::Function, "not_test", 6),
                    leaf(SourceTypeKind::Function, "unix_featured", 8),
                    leaf(SourceTypeKind::Type, "Derived", 10),
                    leaf(SourceTypeKind::Function, "bare", 12),
                    leaf(SourceTypeKind::Function, "invalid", 14),
                ])
            );
        }

        #[test]
        fn parse_source_should_map_impl_of_external_type() {
            let source = "\
impl External {
    fn run() {}
}
impl (External) {
    fn walk() {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    SourceTypeKind::Type,
                    "External",
                    1,
                    vec![
                        leaf(SourceTypeKind::Method, "run", 2),
                        leaf(SourceTypeKind::Method, "walk", 5),
                    ]
                )])
            );
        }

        #[test]
        fn parse_source_should_map_test_modules_and_tests() {
            let source = "\
struct S;
#[cfg(test)]
mod tests {
    use super::*;
    fn helper() {}
    struct Mock;
    #[test]
    fn top_should_work() {}
    mod s {
        #[test]
        fn s_should_work() {}
        #[tokio::test]
        async fn s_async_should_work() {}
    }
    mod fixtures {
        pub fn data() {}
    }
    mod outer {
        mod inner {
            #[test]
            fn inner_should_work() {}
        }
    }
    mod external;
    #[my::deep::test]
    fn deep() {}
}
#[cfg(test)]
mod empty {}
#[cfg(test)]
#[test]
fn loose_should_work() {}
#[cfg(all(test, unix))]
mod unix_tests;
#[cfg(test)]
mod test_utils {
    pub fn make() {}
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Type, "S", 1),
                    ty(
                        TEST_MODULE,
                        "tests",
                        3,
                        vec![
                            leaf(SourceTypeKind::Test, "top_should_work", 8),
                            ty(
                                TEST_MODULE,
                                "s",
                                9,
                                vec![
                                    leaf(SourceTypeKind::Test, "s_should_work", 11),
                                    leaf(SourceTypeKind::Test, "s_async_should_work", 13),
                                ]
                            ),
                            leaf(HELPER_MODULE, "fixtures", 15),
                            ty(
                                TEST_MODULE,
                                "outer",
                                18,
                                vec![ty(
                                    TEST_MODULE,
                                    "inner",
                                    19,
                                    vec![leaf(SourceTypeKind::Test, "inner_should_work", 21)]
                                )]
                            ),
                            leaf(SourceTypeKind::Test, "deep", 26),
                        ]
                    ),
                    leaf(HELPER_MODULE, "empty", 29),
                    leaf(SourceTypeKind::Test, "loose_should_work", 32),
                    leaf(HELPER_MODULE, "test_utils", 36),
                ])
            );
        }

        #[test]
        fn parse_source_should_map_tests_outside_test_context() {
            let source = "\
#[test]
fn smoke() {}
mod tests {
    #[test]
    fn a() {}
    mod b {
        #[test]
        fn b_should_work() {}
    }
}
mod mixed {
    fn setup() {}
    #[test]
    fn c() {}
}
#[my::deep::test]
fn deep() {}
mod runtime_only {
    #[tokio::test]
    async fn t() {}
}
#[cfg(test)]
#[tokio::test]
async fn gated() {}
mod config {
    pub const MAX: u32 = 5;
    pub mod parser;
    #[cfg(test)]
    mod tests {
        #[test]
        fn x() {}
    }
}
pub mod store {
    pub use inner::*;
    #[cfg(test)]
    pub mod mocks {
        pub fn m() {}
    }
}
mod empty {}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Test, "smoke", 2),
                    ty(
                        TEST_MODULE,
                        "tests",
                        3,
                        vec![
                            leaf(SourceTypeKind::Test, "a", 5),
                            ty(
                                TEST_MODULE,
                                "b",
                                6,
                                vec![leaf(SourceTypeKind::Test, "b_should_work", 8)]
                            ),
                        ]
                    ),
                    ty(
                        TEST_MODULE,
                        "mixed",
                        11,
                        vec![leaf(SourceTypeKind::Test, "c", 14)]
                    ),
                    leaf(SourceTypeKind::Function, "deep", 17),
                    ty(
                        MODULE,
                        "runtime_only",
                        18,
                        vec![leaf(SourceTypeKind::Function, "t", 20)]
                    ),
                    leaf(SourceTypeKind::Test, "gated", 24),
                    ty(
                        MODULE,
                        "config",
                        25,
                        vec![ty(
                            TEST_MODULE,
                            "tests",
                            29,
                            vec![leaf(SourceTypeKind::Test, "x", 31)]
                        )]
                    ),
                    ty(MODULE, "store", 34, vec![leaf(HELPER_MODULE, "mocks", 37)]),
                    leaf(MODULE, "empty", 41),
                ])
            );
        }

        #[test]
        fn parse_source_should_map_tests_of_inline_modules_in_module() {
            let source = "\
mod outer {
    struct Inner;
    #[cfg(test)]
    mod tests {
        #[test]
        fn inner_should_work() {}
    }
}
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![ty(
                    MODULE,
                    "outer",
                    1,
                    vec![
                        leaf(SourceTypeKind::Type, "Inner", 2),
                        ty(
                            TEST_MODULE,
                            "tests",
                            4,
                            vec![leaf(SourceTypeKind::Test, "inner_should_work", 6)]
                        ),
                    ]
                )])
            );
        }

        #[test]
        fn parse_source_test_only_file_should_map_test_items() {
            let source = "\
#![cfg(test)]
struct Mock;
#[test]
fn file_should_work() {}
mod nested { mod deeper {} }
";
            assert_eq!(
                parse_source(source).unwrap(),
                types(vec![
                    leaf(SourceTypeKind::Test, "file_should_work", 4),
                    ty(
                        HELPER_MODULE,
                        "nested",
                        5,
                        vec![leaf(HELPER_MODULE, "deeper", 5)]
                    ),
                ])
            );
        }

        #[test]
        fn parse_source_empty_should_return_no_types() {
            assert_eq!(parse_source("").unwrap(), SourceTypes::default());
        }

        #[test]
        fn parse_source_invalid_should_fail_with_location() {
            for (source, location) in [
                ("struct S;\nfn () {}", "2:4: expected identifier"),
                ("struct S;\nstruct T", "2:9: unexpected end of input"),
                ("struct S;\nfn\n", "3:1: unexpected end of input"),
            ] {
                let error = parse_source(source).unwrap_err();
                assert!(error.starts_with(location), "{source:?}: {error}");
            }
        }
    }
}
