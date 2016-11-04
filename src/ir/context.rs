//! Common context that is passed around during parsing and codegen.

use BindgenOptions;
use clang::{self, Cursor};
use parse::ClangItemParser;
use std::borrow::{Borrow, Cow};
use std::collections::{HashMap, HashSet, hash_map};
use std::collections::btree_map::{self, BTreeMap};
use std::fmt;
use super::int::IntKind;
use super::item::{Item, ItemCanonicalName, ItemId};
use super::item_kind::ItemKind;
use super::module::Module;
use super::ty::{FloatKind, Type, TypeKind};
use super::type_collector::{ItemSet, TypeCollector};
use syntax::ast::Ident;
use syntax::codemap::{DUMMY_SP, Span};
use syntax::ext::base::ExtCtxt;

/// A key used to index a resolved type, so we only process it once.
///
/// This is almost always a USR string (an unique identifier generated by
/// clang), but it can also be the canonical declaration if the type is unnamed,
/// in which case clang may generate the same USR for multiple nested unnamed
/// types.
#[derive(Eq, PartialEq, Hash, Debug)]
enum TypeKey {
    USR(String),
    Declaration(Cursor),
}

// This is just convenience to avoid creating a manual debug impl for the
// context.
struct GenContext<'ctx>(ExtCtxt<'ctx>);

impl<'ctx> fmt::Debug for GenContext<'ctx> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "GenContext {{ ... }}")
    }
}

/// A context used during parsing and generation of structs.
#[derive(Debug)]
pub struct BindgenContext<'ctx> {
    /// The map of all the items parsed so far.
    ///
    /// It's a BTreeMap because we want the keys to be sorted to have consistent
    /// output.
    items: BTreeMap<ItemId, Item>,

    /// Clang USR to type map. This is needed to be able to associate types with
    /// item ids during parsing.
    types: HashMap<TypeKey, ItemId>,

    /// A cursor to module map. Similar reason than above.
    modules: HashMap<Cursor, ItemId>,

    /// The root module, this is guaranteed to be an item of kind Module.
    root_module: ItemId,

    /// Current module being traversed.
    current_module: ItemId,

    /// A stack with the current type declarations and types we're parsing. This
    /// is needed to avoid infinite recursion when parsing a type like:
    ///
    /// struct c { struct c* next; };
    ///
    /// This means effectively, that a type has a potential ID before knowing if
    /// it's a correct type. But that's not important in practice.
    ///
    /// We could also use the `types` HashMap, but my intention with it is that
    /// only valid types and declarations end up there, and this could
    /// potentially break that assumption.
    ///
    /// FIXME: Should not be public, though... meh.
    pub currently_parsed_types: Vec<(Cursor, ItemId)>,

    /// A HashSet with all the already parsed macro names. This is done to avoid
    /// hard errors while parsing duplicated macros.
    parsed_macros: HashSet<String>,

    /// The active replacements collected from replaces="xxx" annotations.
    replacements: HashMap<String, ItemId>,

    collected_typerefs: bool,

    /// Dummy structures for code generation.
    gen_ctx: Option<&'ctx GenContext<'ctx>>,
    span: Span,

    /// The clang index for parsing.
    index: clang::Index,

    /// The translation unit for parsing.
    translation_unit: clang::TranslationUnit,

    /// The options given by the user via cli or other medium.
    options: BindgenOptions,
}

impl<'ctx> BindgenContext<'ctx> {
    /// Construct the context for the given `options`.
    pub fn new(options: BindgenOptions) -> Self {
        use clangll;

        let index = clang::Index::new(false, true);

        let parse_options =
            clangll::CXTranslationUnit_DetailedPreprocessingRecord;
        let translation_unit =
            clang::TranslationUnit::parse(&index,
                                          "",
                                          &options.clang_args,
                                          &[],
                                          parse_options)
                .expect("TranslationUnit::parse");

        let root_module = Self::build_root_module();
        let mut me = BindgenContext {
            items: Default::default(),
            types: Default::default(),
            modules: Default::default(),
            root_module: root_module.id(),
            current_module: root_module.id(),
            currently_parsed_types: vec![],
            parsed_macros: Default::default(),
            replacements: Default::default(),
            collected_typerefs: false,
            gen_ctx: None,
            span: DUMMY_SP,
            index: index,
            translation_unit: translation_unit,
            options: options,
        };

        me.add_item(root_module, None, None);

        me
    }

    /// Define a new item.
    ///
    /// This inserts it into the internal items set, and its type into the
    /// internal types set.
    pub fn add_item(&mut self,
                    item: Item,
                    declaration: Option<Cursor>,
                    location: Option<Cursor>) {
        use clangll::{CXCursor_ClassTemplate,
                      CXCursor_ClassTemplatePartialSpecialization};
        debug!("BindgenContext::add_item({:?}, declaration: {:?}, loc: {:?}",
               item,
               declaration,
               location);
        debug_assert!(declaration.is_some() || !item.kind().is_type() ||
                      item.kind().expect_type().is_builtin_or_named(),
                      "Adding a type without declaration?");

        let id = item.id();
        let is_type = item.kind().is_type();
        let is_unnamed = is_type && item.expect_type().name().is_none();
        let old_item = self.items.insert(id, item);
        assert!(old_item.is_none(), "Inserted type twice?");

        // Unnamed items can have an USR, but they can't be referenced from
        // other sites explicitly and the USR can match if the unnamed items are
        // nested, so don't bother tracking them.
        if is_type && declaration.is_some() {
            let mut declaration = declaration.unwrap();
            if !declaration.is_valid() {
                if let Some(location) = location {
                    if location.kind() == CXCursor_ClassTemplate ||
                       location.kind() ==
                       CXCursor_ClassTemplatePartialSpecialization {
                        declaration = location;
                    }
                }
            }
            declaration = declaration.canonical();
            if !declaration.is_valid() {
                // This could happen, for example, with types like `int*` or
                // similar.
                //
                // Fortunately, we don't care about those types being
                // duplicated, so we can just ignore them.
                debug!("Invalid declaration {:?} found for type {:?}",
                       declaration,
                       self.items.get(&id).unwrap().kind().expect_type());
                return;
            }

            let key = if is_unnamed {
                TypeKey::Declaration(declaration)
            } else if let Some(usr) = declaration.usr() {
                TypeKey::USR(usr)
            } else {
                error!("Valid declaration with no USR: {:?}, {:?}",
                       declaration,
                       location);
                return;
            };

            let old = self.types.insert(key, id);
            debug_assert_eq!(old, None);
        }
    }

    // TODO: Move all this syntax crap to other part of the code.

    /// Given that we are in the codegen phase, get the syntex context.
    pub fn ext_cx(&self) -> &ExtCtxt<'ctx> {
        &self.gen_ctx.expect("Not in gen phase").0
    }

    /// Given that we are in the codegen phase, get the current syntex span.
    pub fn span(&self) -> Span {
        self.span
    }

    /// Mangles a name so it doesn't conflict with any keyword.
    pub fn rust_mangle<'a>(&self, name: &'a str) -> Cow<'a, str> {
        use syntax::parse::token;
        let ident = self.rust_ident_raw(&name);
        let token = token::Ident(ident);
        if token.is_any_keyword() || name.contains("@") ||
           name.contains("?") || name.contains("$") ||
           "bool" == name {
            let mut s = name.to_owned();
            s = s.replace("@", "_");
            s = s.replace("?", "_");
            s = s.replace("$", "_");
            s.push_str("_");
            return Cow::Owned(s);
        }
        Cow::Borrowed(name)
    }

    /// Returns a mangled name as a rust identifier.
    pub fn rust_ident(&self, name: &str) -> Ident {
        self.rust_ident_raw(&self.rust_mangle(name))
    }

    /// Returns a mangled name as a rust identifier.
    pub fn rust_ident_raw<S>(&self, name: &S) -> Ident
        where S: Borrow<str>,
    {
        self.ext_cx().ident_of(name.borrow())
    }

    /// Iterate over all items that have been defined.
    pub fn items<'a>(&'a self) -> btree_map::Iter<'a, ItemId, Item> {
        self.items.iter()
    }

    /// Have we collected all unresolved type references yet?
    pub fn collected_typerefs(&self) -> bool {
        self.collected_typerefs
    }

    /// Gather all the unresolved type references.
    fn collect_typerefs
        (&mut self)
         -> Vec<(ItemId, clang::Type, Option<clang::Cursor>, Option<ItemId>)> {
        debug_assert!(!self.collected_typerefs);
        self.collected_typerefs = true;
        let mut typerefs = vec![];
        for (id, ref mut item) in &mut self.items {
            let kind = item.kind();
            let ty = match kind.as_type() {
                Some(ty) => ty,
                None => continue,
            };

            match *ty.kind() {
                TypeKind::UnresolvedTypeRef(ref ty, loc, parent_id) => {
                    typerefs.push((*id, ty.clone(), loc, parent_id));
                }
                _ => {}
            };
        }
        typerefs
    }

    /// Collect all of our unresolved type references and resolve them.
    fn resolve_typerefs(&mut self) {
        let typerefs = self.collect_typerefs();

        for (id, ty, loc, parent_id) in typerefs {
            let _resolved = {
                let resolved = Item::from_ty(&ty, loc, parent_id, self)
                    .expect("What happened?");
                let mut item = self.items.get_mut(&id).unwrap();

                *item.kind_mut().as_type_mut().unwrap().kind_mut() =
                    TypeKind::ResolvedTypeRef(resolved);
                resolved
            };

            // Something in the STL is trolling me. I don't need this assertion
            // right now, but worth investigating properly once this lands.
            //
            // debug_assert!(self.items.get(&resolved).is_some(), "How?");
        }
    }

    /// Iterate over all items and replace any item that has been named in a
    /// `replaces="SomeType"` annotation with the replacement type.
    fn process_replacements(&mut self) {
        if self.replacements.is_empty() {
            debug!("No replacements to process");
            return;
        }

        // FIXME: This is linear, but the replaces="xxx" annotation was already
        // there, and for better or worse it's useful, sigh...
        //
        // We leverage the ResolvedTypeRef thing, though, which is cool :P.

        let mut replacements = vec![];

        for (id, item) in self.items.iter() {
            // Calls to `canonical_name` are expensive, so eagerly filter out
            // items that cannot be replaced.
            let ty = match item.kind().as_type() {
                Some(ty) => ty,
                None => continue,
            };

            match *ty.kind() {
                TypeKind::Comp(ref ci) if !ci.is_template_specialization() => {}
                TypeKind::TemplateAlias(_, _) |
                TypeKind::Alias(_, _) => {}
                _ => continue,
            }

            let name = item.real_canonical_name(self,
                                                self.options()
                                                    .enable_cxx_namespaces,
                                                true);
            let replacement = self.replacements.get(&name);

            if let Some(replacement) = replacement {
                if replacement != id {
                    // We set this just after parsing the annotation. It's
                    // very unlikely, but this can happen.
                    if self.items.get(replacement).is_some() {
                        replacements.push((*id, *replacement));
                    }
                }
            }
        }

        for (id, replacement) in replacements {
            debug!("Replacing {:?} with {:?}", id, replacement);

            let mut item = self.items.get_mut(&id).unwrap();
            *item.kind_mut().as_type_mut().unwrap().kind_mut() =
                TypeKind::ResolvedTypeRef(replacement);
        }
    }

    /// Enter the code generation phase, invoke the given callback `cb`, and
    /// leave the code generation phase.
    pub fn gen<F, Out>(&mut self, cb: F) -> Out
        where F: FnOnce(&Self) -> Out,
    {
        use syntax::ext::expand::ExpansionConfig;
        use syntax::codemap::{ExpnInfo, MacroBang, NameAndSpan};
        use syntax::ext::base;
        use syntax::parse;
        use std::mem;

        let cfg = ExpansionConfig::default("xxx".to_owned());
        let sess = parse::ParseSess::new();
        let mut loader = base::DummyResolver;
        let mut ctx =
            GenContext(base::ExtCtxt::new(&sess, vec![], cfg, &mut loader));

        ctx.0.bt_push(ExpnInfo {
            call_site: self.span,
            callee: NameAndSpan {
                format: MacroBang(parse::token::intern("")),
                allow_internal_unstable: false,
                span: None,
            },
        });

        // FIXME: This is evil, we should move code generation to use a wrapper
        // of BindgenContext instead, I guess. Even though we know it's fine
        // because we remove it before the end of this function.
        self.gen_ctx = Some(unsafe { mem::transmute(&ctx) });

        if !self.collected_typerefs() {
            self.resolve_typerefs();
            self.process_replacements();
        }

        let ret = cb(self);
        self.gen_ctx = None;
        ret
    }

    // This deserves a comment. Builtin types don't get a valid declaration, so
    // we can't add it to the cursor->type map.
    //
    // That being said, they're not generated anyway, and are few, so the
    // duplication and special-casing is fine.
    //
    // If at some point we care about the memory here, probably a map TypeKind
    // -> builtin type ItemId would be the best to improve that.
    fn add_builtin_item(&mut self, item: Item) {
        debug_assert!(item.kind().is_type());
        let id = item.id();
        let old_item = self.items.insert(id, item);
        assert!(old_item.is_none(), "Inserted type twice?");
    }

    fn build_root_module() -> Item {
        let module = Module::new(Some("root".into()));
        let id = ItemId::next();
        Item::new(id, None, None, id, ItemKind::Module(module))
    }

    /// Get the root module.
    pub fn root_module(&self) -> ItemId {
        self.root_module
    }

    /// Resolve the given `ItemId` as a type.
    ///
    /// Panics if there is no item for the given `ItemId` or if the resolved
    /// item is not a `Type`.
    pub fn resolve_type(&self, type_id: ItemId) -> &Type {
        self.items.get(&type_id).unwrap().kind().expect_type()
    }

    /// Resolve the given `ItemId` as a type, or `None` if there is no item with
    /// the given id.
    ///
    /// Panics if the id resolves to an item that is not a type.
    pub fn safe_resolve_type(&self, type_id: ItemId) -> Option<&Type> {
        self.items.get(&type_id).map(|t| t.kind().expect_type())
    }

    /// Resolve the given `ItemId` into an `Item`, or `None` if no such item
    /// exists.
    pub fn resolve_item_fallible(&self, item_id: ItemId) -> Option<&Item> {
        self.items.get(&item_id)
    }

    /// Resolve the given `ItemId` into an `Item`.
    ///
    /// Panics if the given id does not resolve to any item.
    pub fn resolve_item(&self, item_id: ItemId) -> &Item {
        match self.items.get(&item_id) {
            Some(item) => item,
            None => panic!("Not an item: {:?}", item_id),
        }
    }

    /// Get the current module.
    pub fn current_module(&self) -> ItemId {
        self.current_module
    }

    /// This is one of the hackiest methods in all the parsing code. This method
    /// is used to allow having templates with another argument names instead of
    /// the canonical ones.
    ///
    /// This is surprisingly difficult to do with libclang, due to the fact that
    /// partial template specializations don't provide explicit template
    /// argument information.
    ///
    /// The only way to do this as far as I know, is inspecting manually the
    /// AST, looking for TypeRefs inside. This, unfortunately, doesn't work for
    /// more complex cases, see the comment on the assertion below.
    ///
    /// To see an example of what this handles:
    ///
    /// ```c++
    ///     template<typename T>
    ///     class Incomplete {
    ///       T p;
    ///     };
    ///
    ///     template<typename U>
    ///     class Foo {
    ///       Incomplete<U> bar;
    ///     };
    /// ```
    fn build_template_wrapper(&mut self,
                              with_id: ItemId,
                              wrapping: ItemId,
                              parent_id: ItemId,
                              ty: &clang::Type,
                              location: clang::Cursor)
                              -> ItemId {
        use clangll::*;
        let mut args = vec![];
        let mut found_invalid_template_ref = false;
        location.visit(|c| {
            if c.kind() == CXCursor_TemplateRef &&
               c.cur_type().kind() == CXType_Invalid {
                found_invalid_template_ref = true;
            }
            if c.kind() == CXCursor_TypeRef {
                // The `with_id` id will potentially end up unused if we give up
                // on this type (for example, its a tricky partial template
                // specialization), so if we pass `with_id` as the parent, it is
                // potentially a dangling reference. Instead, use the canonical
                // template declaration as the parent. It is already parsed and
                // has a known-resolvable `ItemId`.
                let new_ty = Item::from_ty_or_ref(c.cur_type(),
                                                  Some(c),
                                                  Some(wrapping),
                                                  self);
                args.push(new_ty);
            }
            CXChildVisit_Continue
        });

        let item = {
            let wrapping_type = self.resolve_type(wrapping);
            let old_args = match *wrapping_type.kind() {
                TypeKind::Comp(ref ci) => ci.template_args(),
                _ => panic!("how?"),
            };
            // The following assertion actually fails with partial template
            // specialization. But as far as I know there's no way at all to
            // grab the specialized types from neither the AST or libclang.
            //
            // This flaw was already on the old parser, but I now think it has
            // no clear solution.
            //
            // For an easy example in which there's no way at all of getting the
            // `int` type, except manually parsing the spelling:
            //
            //     template<typename T, typename U>
            //     class Incomplete {
            //       T d;
            //       U p;
            //     };
            //
            //     template<typename U>
            //     class Foo {
            //       Incomplete<U, int> bar;
            //     };
            //
            // debug_assert_eq!(old_args.len(), args.len());
            //
            // That being said, this is not so common, so just error! and hope
            // for the best, returning the previous type, who knows.
            if old_args.len() != args.len() {
                error!("Found partial template specialization, \
                        expect dragons!");
                return wrapping;
            }

            let type_kind = TypeKind::TemplateRef(wrapping, args);
            let name = ty.spelling();
            let name = if name.is_empty() { None } else { Some(name) };
            let ty = Type::new(name,
                               ty.fallible_layout().ok(),
                               type_kind,
                               ty.is_const());
            Item::new(with_id, None, None, parent_id, ItemKind::Type(ty))
        };

        // Bypass all the validations in add_item explicitly.
        self.items.insert(with_id, item);
        with_id
    }

    /// Looks up for an already resolved type, either because it's builtin, or
    /// because we already have it in the map.
    pub fn builtin_or_resolved_ty(&mut self,
                                  with_id: ItemId,
                                  parent_id: Option<ItemId>,
                                  ty: &clang::Type,
                                  location: Option<clang::Cursor>)
                                  -> Option<ItemId> {
        use clangll::{CXCursor_ClassTemplate,
                      CXCursor_ClassTemplatePartialSpecialization};
        debug!("builtin_or_resolved_ty: {:?}, {:?}, {:?}",
               ty,
               location,
               parent_id);
        let mut declaration = ty.declaration();
        if !declaration.is_valid() {
            if let Some(location) = location {
                if location.kind() == CXCursor_ClassTemplate ||
                   location.kind() ==
                   CXCursor_ClassTemplatePartialSpecialization {
                    declaration = location;
                }
            }
        }
        let canonical_declaration = declaration.canonical();
        if canonical_declaration.is_valid() {
            let id = self.types
                .get(&TypeKey::Declaration(canonical_declaration))
                .map(|id| *id)
                .or_else(|| {
                    canonical_declaration.usr()
                        .and_then(|usr| self.types.get(&TypeKey::USR(usr)))
                        .map(|id| *id)
                });
            if let Some(id) = id {
                debug!("Already resolved ty {:?}, {:?}, {:?} {:?}",
                       id,
                       declaration,
                       ty,
                       location);

                // If the declaration existed, we *might* be done, but it's not
                // the case for class templates, where the template arguments
                // may vary.
                //
                // In this case, we create a TemplateRef with the new template
                // arguments, pointing to the canonical template.
                //
                // Note that we only do it if parent_id is some, and we have a
                // location for building the new arguments, the template
                // argument names don't matter in the global context.
                if (declaration.kind() == CXCursor_ClassTemplate ||
                    declaration.kind() ==
                    CXCursor_ClassTemplatePartialSpecialization) &&
                   *ty != canonical_declaration.cur_type() &&
                   location.is_some() &&
                   parent_id.is_some() {
                    return Some(self.build_template_wrapper(with_id,
                                                id,
                                                parent_id.unwrap(),
                                                ty,
                                                location.unwrap()));
                }

                return Some(self.build_ty_wrapper(with_id, id, parent_id, ty));
            }
        }

        debug!("Not resolved, maybe builtin?");

        // Else, build it.
        self.build_builtin_ty(ty, declaration)
    }

    // This is unfortunately a lot of bloat, but is needed to properly track
    // constness et. al.
    //
    // We should probably make the constness tracking separate, so it doesn't
    // bloat that much, but hey, we already bloat the heck out of builtin types.
    fn build_ty_wrapper(&mut self,
                        with_id: ItemId,
                        wrapped_id: ItemId,
                        parent_id: Option<ItemId>,
                        ty: &clang::Type)
                        -> ItemId {
        let spelling = ty.spelling();
        let is_const = ty.is_const();
        let layout = ty.fallible_layout().ok();
        let type_kind = TypeKind::ResolvedTypeRef(wrapped_id);
        let ty = Type::new(Some(spelling), layout, type_kind, is_const);
        let item = Item::new(with_id,
                             None,
                             None,
                             parent_id.unwrap_or(self.current_module),
                             ItemKind::Type(ty));
        self.add_builtin_item(item);
        with_id
    }

    fn build_builtin_ty(&mut self,
                        ty: &clang::Type,
                        _declaration: Cursor)
                        -> Option<ItemId> {
        use clangll::*;
        let type_kind = match ty.kind() {
            CXType_NullPtr => TypeKind::NullPtr,
            CXType_Void => TypeKind::Void,
            CXType_Bool => TypeKind::Int(IntKind::Bool),
            CXType_Int => TypeKind::Int(IntKind::Int),
            CXType_UInt => TypeKind::Int(IntKind::UInt),
            CXType_SChar | CXType_Char_S => TypeKind::Int(IntKind::Char),
            CXType_UChar | CXType_Char_U => TypeKind::Int(IntKind::UChar),
            CXType_Short => TypeKind::Int(IntKind::Short),
            CXType_UShort => TypeKind::Int(IntKind::UShort),
            CXType_WChar | CXType_Char16 => TypeKind::Int(IntKind::U16),
            CXType_Char32 => TypeKind::Int(IntKind::U32),
            CXType_Long => TypeKind::Int(IntKind::Long),
            CXType_ULong => TypeKind::Int(IntKind::ULong),
            CXType_LongLong => TypeKind::Int(IntKind::LongLong),
            CXType_ULongLong => TypeKind::Int(IntKind::ULongLong),
            CXType_Int128 => TypeKind::Int(IntKind::I128),
            CXType_UInt128 => TypeKind::Int(IntKind::U128),
            CXType_Float => TypeKind::Float(FloatKind::Float),
            CXType_Double => TypeKind::Float(FloatKind::Double),
            CXType_LongDouble => TypeKind::Float(FloatKind::LongDouble),
            _ => return None,
        };

        let spelling = ty.spelling();
        let is_const = ty.is_const();
        let layout = ty.fallible_layout().ok();
        let ty = Type::new(Some(spelling), layout, type_kind, is_const);
        let id = ItemId::next();
        let item =
            Item::new(id, None, None, self.root_module, ItemKind::Type(ty));
        self.add_builtin_item(item);
        Some(id)
    }

    /// Get the current Clang translation unit that is being processed.
    pub fn translation_unit(&self) -> &clang::TranslationUnit {
        &self.translation_unit
    }

    /// Have we parsed the macro named `macro_name` already?
    pub fn parsed_macro(&self, macro_name: &str) -> bool {
        self.parsed_macros.contains(macro_name)
    }

    /// Mark the macro named `macro_name` as parsed.
    pub fn note_parsed_macro(&mut self, macro_name: String) {
        debug_assert!(!self.parsed_macros.contains(&macro_name));
        self.parsed_macros.insert(macro_name);
    }

    /// Are we in the codegen phase?
    pub fn in_codegen_phase(&self) -> bool {
        self.gen_ctx.is_some()
    }

    /// Mark the type with the given `name` as replaced by the type with id
    /// `potential_ty`.
    ///
    /// Replacement types are declared using the `replaces="xxx"` annotation,
    /// and implies that the original type is hidden.
    pub fn replace(&mut self, name: &str, potential_ty: ItemId) {
        match self.replacements.entry(name.into()) {
            hash_map::Entry::Vacant(entry) => {
                debug!("Defining replacement for {} as {:?}",
                       name,
                       potential_ty);
                entry.insert(potential_ty);
            }
            hash_map::Entry::Occupied(occupied) => {
                warn!("Replacement for {} already defined as {:?}; \
                       ignoring duplicate replacement definition as {:?}}}",
                      name,
                      occupied.get(),
                      potential_ty);
            }
        }
    }

    /// Is the item with the given `name` hidden? Or is the item with the given
    /// `name` and `id` replaced by another type, and effectively hidden?
    pub fn hidden_by_name(&self, name: &str, id: ItemId) -> bool {
        debug_assert!(self.in_codegen_phase(),
                      "You're not supposed to call this yet");
        self.options.hidden_types.contains(name) ||
        self.is_replaced_type(name, id)
    }

    /// Has the item with the given `name` and `id` been replaced by another
    /// type?
    pub fn is_replaced_type(&self, name: &str, id: ItemId) -> bool {
        match self.replacements.get(name) {
            Some(replaced_by) if *replaced_by != id => true,
            _ => false,
        }
    }

    /// Is the type with the given `name` marked as opaque?
    pub fn opaque_by_name(&self, name: &str) -> bool {
        debug_assert!(self.in_codegen_phase(),
                      "You're not supposed to call this yet");
        self.options.opaque_types.contains(name)
    }

    /// Get the options used to configure this bindgen context.
    pub fn options(&self) -> &BindgenOptions {
        &self.options
    }

    /// Given a CXCursor_Namespace cursor, return the item id of the
    /// corresponding module, or create one on the fly.
    pub fn module(&mut self, cursor: clang::Cursor) -> ItemId {
        use clangll::*;
        assert!(cursor.kind() == CXCursor_Namespace, "Be a nice person");
        let cursor = cursor.canonical();
        let module_id = match self.modules.get(&cursor) {
            Some(id) => return *id,
            None => ItemId::next(),
        };

        let module_name = self.translation_unit
            .tokens(&cursor)
            .and_then(|tokens| {
                if tokens.len() <= 1 {
                    None
                } else {
                    match &*tokens[1].spelling {
                        "{" => None,
                        s => Some(s.to_owned()),
                    }
                }
            });

        let module = Module::new(module_name);
        let module = Item::new(module_id,
                               None,
                               None,
                               self.current_module,
                               ItemKind::Module(module));

        self.add_item(module, None, None);

        module_id
    }

    /// Start traversing the module with the given `module_id`, invoke the
    /// callback `cb`, and then return to traversing the original module.
    pub fn with_module<F>(&mut self, module_id: ItemId, cb: F)
        where F: FnOnce(&mut Self, &mut Vec<ItemId>),
    {
        debug_assert!(self.resolve_item(module_id).kind().is_module(), "Wat");

        let previous_id = self.current_module;
        self.current_module = module_id;

        let mut children = vec![];
        cb(self, &mut children);

        self.items
            .get_mut(&module_id)
            .unwrap()
            .as_module_mut()
            .expect("Not a module?")
            .children_mut()
            .extend(children.into_iter());

        self.current_module = previous_id;
    }

    /// Iterate over all (explicitly or transitively) whitelisted items.
    ///
    /// If no items are explicitly whitelisted, then all items are considered
    /// whitelisted.
    pub fn whitelisted_items<'me>(&'me self)
                                  -> WhitelistedItemsIter<'me, 'ctx> {
        assert!(self.in_codegen_phase());
        assert!(self.current_module == self.root_module);

        let roots = self.items()
            .filter(|&(_, item)| {
                // If nothing is explicitly whitelisted, then everything is fair
                // game.
                if self.options().whitelisted_types.is_empty() &&
                   self.options().whitelisted_functions.is_empty() &&
                   self.options().whitelisted_vars.is_empty() {
                    return true;
                }

                let name = item.canonical_name(self);
                match *item.kind() {
                    ItemKind::Module(..) => false,
                    ItemKind::Function(_) => {
                        self.options().whitelisted_functions.matches(&name)
                    }
                    ItemKind::Var(_) => {
                        self.options().whitelisted_vars.matches(&name)
                    }
                    ItemKind::Type(ref ty) => {
                        if self.options().whitelisted_types.matches(&name) {
                            return true;
                        }

                        // Unnamed top-level enums are special and we whitelist
                        // them via the `whitelisted_vars` filter, since they're
                        // effectively top-level constants, and there's no way
                        // for them to be referenced consistently.
                        if let TypeKind::Enum(ref enum_) = *ty.kind() {
                            if ty.name().is_none() &&
                               enum_.variants().iter().any(|variant| {
                                self.options()
                                    .whitelisted_vars
                                    .matches(&variant.name())
                            }) {
                                return true;
                            }
                        }

                        false
                    }
                }
            })
            .map(|(&id, _)| id);

        let seen: ItemSet = roots.collect();

        // The .rev() preserves the expected ordering traversal, resulting in
        // more stable-ish bindgen-generated names for anonymous types (like
        // unions).
        let to_iterate = seen.iter().cloned().rev().collect();

        WhitelistedItemsIter {
            ctx: self,
            seen: seen,
            to_iterate: to_iterate,
        }
    }
}

/// An iterator over whitelisted items.
///
/// See `BindgenContext::whitelisted_items` for more information.
pub struct WhitelistedItemsIter<'ctx, 'gen>
    where 'gen: 'ctx,
{
    ctx: &'ctx BindgenContext<'gen>,

    // The set of whitelisted items we have seen. If you think of traversing
    // whitelisted items like GC tracing, this is the mark bits, and contains
    // both black and gray items.
    seen: ItemSet,

    // The set of whitelisted items that we have seen but have yet to iterate
    // over and collect transitive references from. To return to the GC analogy,
    // this is the mark stack, containing the set of gray items which we have
    // not finished tracing yet.
    to_iterate: Vec<ItemId>,
}

impl<'ctx, 'gen> Iterator for WhitelistedItemsIter<'ctx, 'gen>
    where 'gen: 'ctx,
{
    type Item = ItemId;

    fn next(&mut self) -> Option<Self::Item> {
        let id = match self.to_iterate.pop() {
            None => return None,
            Some(id) => id,
        };

        debug_assert!(self.seen.contains(&id));

        let mut sub_types = ItemSet::new();
        id.collect_types(self.ctx, &mut sub_types, &());

        for id in sub_types {
            if self.seen.insert(id) {
                self.to_iterate.push(id);
            }
        }

        Some(id)
    }
}
