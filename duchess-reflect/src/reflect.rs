use std::{cell::RefCell, collections::BTreeMap, process::Command, sync::Arc};

use proc_macro2::Span;

use crate::{
    argument::{DuchessDeclaration, Ident, JavaPackage, MethodSelector},
    class_info::{
        ClassDeclKind, ClassInfo, ClassInfoAccessors, ClassKind, ClassRef, Constructor, DotId,
        Field, Flags, Generic, Id, Method, RootMap, SpannedPackageInfo, Type,
    },
    config::Configuration,
    upcasts::Upcasts,
};

impl DuchessDeclaration {
    pub fn to_root_map(&self, reflector: &mut Reflector) -> syn::Result<RootMap> {
        let mut subpackages = BTreeMap::new();
        let mut classes = BTreeMap::new();
        for package in &self.packages {
            package.to_spanned_packages(
                &package.package_name.ids,
                reflector,
                &mut subpackages,
                &mut classes,
            )?;
        }

        let upcasts: Upcasts = Upcasts::from_iter(classes.values().map(|v| &**v));

        Ok(RootMap {
            subpackages,
            classes,
            upcasts,
        })
    }
}

impl JavaPackage {
    fn to_spanned_packages(
        &self,
        name: &[Ident],
        reflector: &mut Reflector,
        map: &mut BTreeMap<Id, SpannedPackageInfo>,
        classes: &mut BTreeMap<DotId, Arc<ClassInfo>>,
    ) -> syn::Result<()> {
        let (first, rest) = name.split_first().unwrap();

        let package_info = || SpannedPackageInfo {
            name: first.to_id(),
            span: first.span,
            subpackages: Default::default(),
            classes: Default::default(),
        };

        let first_id = first.to_id();

        // As written, this allows the same package more than once. I don't see any reason to forbid it,
        // but maybe we want to?
        let parent = map.entry(first_id).or_insert_with(package_info);

        if rest.is_empty() {
            self.insert_classes_into_root_map(reflector, parent, classes)
        } else {
            self.to_spanned_packages(rest, reflector, &mut parent.subpackages, classes)
        }
    }

    fn insert_classes_into_root_map(
        &self,
        reflector: &mut Reflector,
        package: &mut SpannedPackageInfo,
        classes: &mut BTreeMap<DotId, Arc<ClassInfo>>,
    ) -> syn::Result<()> {
        for c in &self.classes {
            let (dot_id, info) = match &c.kind {
                ClassDeclKind::Reflected(c) => {
                    let dot_id = self.make_absolute_dot_id(c.span, &c.name)?;
                    let info = reflector.reflect(&dot_id, c.span)?;

                    // We copy over the span and kind for proper error specification and error checking
                    (
                        dot_id,
                        Arc::new(ClassInfo {
                            kind: c.kind,
                            ..info.to_class_info(c.span)
                        }),
                    )
                }
                ClassDeclKind::Specified(c) => {
                    let dot_id = self.make_absolute_dot_id(c.span, &c.name)?;
                    (
                        dot_id.clone(),
                        Arc::new(ClassInfo {
                            name: dot_id,
                            ..c.clone()
                        }),
                    )
                }
            };

            package.classes.push(dot_id.clone());
            classes.insert(dot_id, info);
        }
        Ok(())
    }

    /// The users give classnames that may not include java package information.
    fn make_absolute_dot_id(&self, span: Span, class_dot_id: &DotId) -> syn::Result<DotId> {
        let package_ids: Vec<Id> = self.package_name.ids.iter().map(|n| n.to_id()).collect();

        let (package, class) = class_dot_id.split();

        // If the user just wrote (e.g.) `String`, add the `java.lang` ourselves.
        if package.is_empty() {
            return Ok(DotId::new(&package_ids, &class));
        }

        // Otherwise, check that the package the user wrote matches our name.
        if &package_ids[..] != package {
            return Err(syn::Error::new(
                span,
                format!("expected package `{}`", self.package_name),
            ));
        }

        Ok(class_dot_id.clone())
    }
}

/// Reflection cache. Given fully qualified java class names,
/// look up info about their interfaces.
pub struct Reflector {
    configuration: Configuration,
    classes: RefCell<BTreeMap<DotId, Arc<JavapClassInfo>>>,
}

impl Reflector {
    pub fn new(configuration: &Configuration) -> Self {
        Self {
            configuration: configuration.clone(),
            classes: Default::default(),
        }
    }

    /// Returns the (potentially cached) info about `class_name`;
    pub fn reflect(&self, class_name: &DotId, span: Span) -> syn::Result<Arc<JavapClassInfo>> {
        // yields an error if we cannot reflect on that class.
        if let Some(class) = self.classes.borrow().get(class_name).map(Arc::clone) {
            return Ok(class);
        }

        let mut command = Command::new(self.configuration.bin_path("javap"));

        if let Some(classpath) = self.configuration.classpath() {
            command.arg("-cp").arg(classpath);
        }

        command.arg("-p").arg(format!("{}", class_name));

        let output_or_err = command.output();

        let output = match output_or_err {
            Ok(o) => o,
            Err(err) => {
                return Err(syn::Error::new(
                    span,
                    format!("failed to execute `{command:?}`: {err}"),
                ));
            }
        };

        if !output.status.success() {
            return Err(syn::Error::new(
                span,
                format!(
                    "unsuccessful execution of `{command:?}` (exit status: {}): {}",
                    output.status,
                    String::from_utf8(output.stderr).unwrap_or(String::from("error"))
                ),
            ));
        }

        let s = match String::from_utf8(output.stdout) {
            Ok(o) => o,
            Err(err) => {
                return Err(syn::Error::new(
                    span,
                    format!("failed to parse output of `{command:?}` as utf-8: {err}"),
                ));
            }
        };

        let ci = ClassInfo::parse(&s, span)?;
        let ci = JavapClassInfo::from(ci);

        // reset the span for the cached data to the call site so that when others look it up,
        // they get the same span.
        Ok(self
            .classes
            .borrow_mut()
            .entry(class_name.clone())
            .or_insert(Arc::new(ci))
            .clone())
    }

    ///
    pub fn reflect_method(&self, method_selector: &MethodSelector) -> syn::Result<ReflectedMethod> {
        match method_selector {
            MethodSelector::ClassName(cn) => {
                let dot_id = cn.to_dot_id();
                let class_info = Arc::new(self.reflect(&dot_id, cn.span)?.to_class_info(cn.span));
                match class_info.constructors.len() {
                    1 => Ok(ReflectedMethod::Constructor(class_info, 0)),
                    0 => Err(syn::Error::new(cn.span, "no constructors found".to_string())),
                    n => Err(syn::Error::new(cn.span, format!("{n} constructors found, use an explicit class declaration to disambiguate")))
                }
            }
            MethodSelector::MethodName(cn, mn) => {
                let dot_id = cn.to_dot_id();
                let class_info = Arc::new(self.reflect(&dot_id, cn.span)?.to_class_info(cn.span));
                let methods: Vec<(MethodIndex, &Method)> = class_info
                    .methods
                    .iter()
                    .enumerate()
                    .filter(|(_i, m)| &m.name[..] == &mn.text[..])
                    .collect();
                match methods.len() {
                    1 => {
                        let (id, _method) = methods[0];
                        Ok(ReflectedMethod::Method(class_info, id))
                    },
                    0 => Err(syn::Error::new(cn.span,  format!("no methods named `{mn}` found"))),
                    n => Err(syn::Error::new(cn.span, format!("{n} methods named `{mn}` found, use an explicit class declaration to disambiguate") )),
                }
            }
            MethodSelector::ClassInfo(_) => todo!(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JavapClassInfo {
    #[allow(dead_code)] // FIXME: replace with `#[expect]` once that stabilizes
    pub flags: Flags,
    pub name: DotId,
    pub kind: ClassKind,
    pub generics: Vec<Generic>,
    pub extends: Vec<ClassRef>,
    pub implements: Vec<ClassRef>,
    pub constructors: Vec<Constructor>,
    pub fields: Vec<Field>,
    pub methods: Vec<Method>,
}

impl ClassInfoAccessors for JavapClassInfo {
    fn flags(&self) -> &Flags {
        &self.flags
    }

    fn name(&self) -> &DotId {
        &self.name
    }

    fn kind(&self) -> ClassKind {
        self.kind
    }

    fn generics(&self) -> &Vec<Generic> {
        &self.generics
    }

    fn extends(&self) -> &Vec<ClassRef> {
        &self.extends
    }

    fn implements(&self) -> &Vec<ClassRef> {
        &self.implements
    }

    fn constructors(&self) -> &Vec<Constructor> {
        &self.constructors
    }

    fn fields(&self) -> &Vec<Field> {
        &self.fields
    }

    fn methods(&self) -> &Vec<Method> {
        &self.methods
    }
}

impl From<ClassInfo> for JavapClassInfo {
    fn from(ci: ClassInfo) -> Self {
        Self {
            flags: ci.flags,
            name: ci.name,
            kind: ci.kind,
            generics: ci.generics,
            extends: ci.extends,
            implements: ci.implements,
            constructors: ci.constructors,
            fields: ci.fields,
            methods: ci.methods,
        }
    }
}

impl JavapClassInfo {
    pub fn to_class_info(&self, span: Span) -> ClassInfo {
        ClassInfo {
            span: span,
            flags: self.flags,
            name: self.name.clone(),
            kind: self.kind,
            generics: self.generics.clone(),
            extends: self.extends.clone(),
            implements: self.implements.clone(),
            constructors: self.constructors.clone(),
            fields: self.fields.clone(),
            methods: self.methods.clone(),
        }
    }
}

pub type ConstructorIndex = usize;
pub type MethodIndex = usize;

/// Reflection on something callable.
#[derive(Clone, Debug)]
pub enum ReflectedMethod {
    Constructor(Arc<ClassInfo>, ConstructorIndex),
    Method(Arc<ClassInfo>, MethodIndex),
}

impl ReflectedMethod {
    /// The name of this callable thing in Rust
    pub fn name(&self) -> Id {
        match self {
            ReflectedMethod::Constructor(..) => Id::from("new"),
            ReflectedMethod::Method(c, m) => c.methods[*m].name.clone(),
        }
    }

    pub fn class(&self) -> &ClassInfo {
        match self {
            ReflectedMethod::Constructor(c, _) => c,
            ReflectedMethod::Method(c, _) => c,
        }
    }

    /// Is this something that is called on a *type*?
    pub fn is_static(&self) -> bool {
        match self {
            ReflectedMethod::Constructor(..) => true,
            ReflectedMethod::Method(c, m) => c.methods[*m].flags.is_static,
        }
    }

    pub fn generics(&self) -> &Vec<Generic> {
        match self {
            ReflectedMethod::Constructor(c, t) => &c.constructors[*t].generics,
            ReflectedMethod::Method(c, m) => &c.methods[*m].generics,
        }
    }

    pub fn argument_tys(&self) -> &Vec<Type> {
        match self {
            ReflectedMethod::Constructor(c, t) => &c.constructors[*t].argument_tys,
            ReflectedMethod::Method(c, m) => &c.methods[*m].argument_tys,
        }
    }
}
