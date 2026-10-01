use std::{borrow::Cow, cell::Cell, cmp::min, collections::HashSet, fmt::Write};

use serde_json::{Map, Value};

use crate::{util::*, *};

macro_rules! prop {
    ($prop:expr) => {
        InstanceToken::Prop(Cow::Borrowed($prop))
    };
}

macro_rules! item {
    ($item:expr) => {
        InstanceToken::Item($item)
    };
}

// VIA patch: a work budget shared by every evaluation of one validation.
/// The work budget of one validation (VIA patch, `VIA-PATCH.md`): one unit
/// per subschema evaluation, units in proportion to the work of `enum`,
/// `const`, `uniqueItems`, `pattern` and `patternProperties`, and a cap on
/// the evaluation depth. Once spent, every pending evaluation returns at
/// once and no further errors are built.
pub(crate) struct Budget {
    left: Cell<u64>,
    max_depth: usize,
    /// The evaluations active now, nested ones (`propertyNames`,
    /// `contentSchema`) included: the depth the cap bounds.
    active: Cell<usize>,
    spent: Cell<bool>,
}

impl Budget {
    pub(crate) fn new(units: u64, max_depth: usize) -> Self {
        Self {
            left: Cell::new(units),
            max_depth,
            active: Cell::new(0),
            spent: Cell::new(false),
        }
    }

    fn unlimited() -> Self {
        Self::new(u64::MAX, usize::MAX)
    }

    /// Takes `units`; false, and spent, when fewer are left.
    pub(crate) fn charge(&self, units: u64) -> bool {
        if self.spent.get() {
            return false;
        }
        match self.left.get().checked_sub(units) {
            Some(left) => {
                self.left.set(left);
                true
            }
            None => {
                self.exhaust();
                false
            }
        }
    }

    /// One more active evaluation; false, and spent, beyond the depth cap.
    fn enter(&self) -> bool {
        let active = self.active.get() + 1;
        self.active.set(active);
        if active > self.max_depth {
            self.exhaust();
        }
        !self.spent()
    }

    fn leave(&self) {
        self.active.set(self.active.get() - 1);
    }

    pub(crate) fn exhaust(&self) {
        self.left.set(0);
        self.spent.set(true);
    }

    pub(crate) fn spent(&self) -> bool {
        self.spent.get()
    }
}

pub(crate) fn validate<'s, 'v>(
    v: &'v Value,
    schema: &'s Schema,
    schemas: &'s Schemas,
) -> Result<(), ValidationError<'s, 'v>> {
    validate_within(v, schema, schemas, &Budget::unlimited(), false)
}

// VIA patch: `validate` with a caller's budget; `bool_result` asks only
// whether `v` is valid, so no error detail is built.
pub(crate) fn validate_within<'s, 'v>(
    v: &'v Value,
    schema: &'s Schema,
    schemas: &'s Schemas,
    budget: &Budget,
    bool_result: bool,
) -> Result<(), ValidationError<'s, 'v>> {
    let scope = Scope {
        sch: schema.idx,
        ref_kw: None,
        vid: 0,
        depth: 0,
        run: 0,
        parent: None,
    };
    let mut vloc = Vec::with_capacity(8);
    let result = Validator {
        v,
        vloc: &mut vloc,
        schema,
        schemas,
        scope,
        uneval: Uneval::from(v, schema, false),
        errors: vec![],
        bool_result,
        budget,
    }
    .validate();
    match result {
        Err(err) => {
            let mut e = ValidationError {
                schema_url: &schema.loc,
                instance_location: InstanceLocation::new(),
                kind: ErrorKind::Schema { url: &schema.loc },
                causes: vec![],
            };
            if let ErrorKind::Group = err.kind {
                e.causes = err.causes;
            } else {
                e.causes.push(err);
            }
            Err(e)
        }
        Ok(_) => Ok(()),
    }
}

macro_rules! kind {
    ($kind:ident, $name:ident: $value:expr) => {
        ErrorKind::$kind { $name: $value }
    };
    ($kind:ident, $got:expr, $want:expr) => {
        ErrorKind::$kind {
            got: $got,
            want: $want,
        }
    };
    ($kind:ident, $got:expr, $want:expr, $err:expr) => {
        ErrorKind::$kind {
            got: $got,
            want: $want,
            err: $err,
        }
    };
    ($kind: ident) => {
        ErrorKind::$kind
    };
}

struct Validator<'v, 's, 'd, 'e> {
    v: &'v Value,
    vloc: &'e mut Vec<InstanceToken<'v>>,
    schema: &'s Schema,
    schemas: &'s Schemas,
    scope: Scope<'d>,
    uneval: Uneval<'v>,
    errors: Vec<ValidationError<'s, 'v>>,
    bool_result: bool, // is interested to know valid or not (but not actuall error)
    budget: &'e Budget, // VIA patch
}

impl<'v, 's> Validator<'v, 's, '_, '_> {
    // VIA patch: every evaluation, nested validations' included, counts
    // against the budget's one depth cap while it is active.
    fn validate(self) -> Result<Uneval<'v>, ValidationError<'s, 'v>> {
        let budget = self.budget;
        if !budget.enter() {
            budget.leave();
            return Err(self.spent_error());
        }
        let result = self.evaluate();
        budget.leave();
        result
    }

    fn evaluate(mut self) -> Result<Uneval<'v>, ValidationError<'s, 'v>> {
        let s = self.schema;
        let v = self.v;

        // VIA patch: one unit per evaluation, plus one per member or item
        // of a container (its scan and evaluation bookkeeping).
        // A member name costs one unit per 64 bytes: it is hashed and may
        // be copied.
        let members = match v {
            Value::Object(obj) => name_units(obj),
            Value::Array(arr) => arr.len() as u64,
            _ => 0,
        };
        // The cycle check below walks the ancestors validating this value.
        let walk = self.scope.run;
        if !self.budget.charge(1 + members + walk as u64) {
            return Err(self.spent_error());
        }

        // boolean --
        if let Some(b) = s.boolean {
            return match b {
                false => Err(self.error(kind!(FalseSchema))),
                true => Ok(self.uneval),
            };
        }

        // check cycle --
        if let Some(scp) = self.scope.check_cycle() {
            if self.bool_result {
                // VIA patch: no keyword locations for a yes/no answer.
                return Err(self.error(kind!(Group)));
            }
            let kind = ErrorKind::RefCycle {
                url: &self.schema.loc,
                kw_loc1: self.kw_loc(&self.scope),
                kw_loc2: self.kw_loc(scp),
            };
            return Err(self.error(kind));
        }

        // type --
        if !s.types.is_empty() {
            let v_type = Type::of(v);
            let matched =
                s.types.contains(v_type) || (s.types.contains(Type::Integer) && is_integer(v));
            if !matched {
                return Err(self.error(kind!(Type, v_type, s.types)));
            }
        }

        // constant --
        if let Some(c) = &s.constant {
            if !equals_within(v, c, self.budget) {
                return Err(self.error(kind!(Const, want: c)));
            }
        }

        // enum --
        if let Some(Enum { types, values }) = &s.enum_ {
            if !types.contains(Type::of(v))
                || !values
                    .iter()
                    .any(|e| self.budget.spent() || equals_within(e, v, self.budget))
            {
                return Err(self.error(kind!(Enum, want: values)));
            }
        }

        // format --
        if let Some(format) = &s.format {
            if let Err(e) = (format.func)(v) {
                self.add_error(kind!(Format, Cow::Borrowed(v), format.name, e));
            }
        }

        // $ref --
        if let Some(ref_) = s.ref_ {
            let result = self.validate_ref(ref_, "$ref");
            if s.draft_version < 2019 {
                return result.map(|_| self.uneval);
            }
            self.errors.extend(result.err());
        }

        // VIA patch: a spent budget ends every pending evaluation.
        if self.budget.spent() {
            return Err(self.spent_error());
        }

        // type specific validations --
        match v {
            Value::Object(obj) => self.obj_validate(obj),
            Value::Array(arr) => self.arr_validate(arr),
            Value::String(str) => self.str_validate(str),
            Value::Number(num) => self.num_validate(num),
            _ => {}
        }

        if self.errors.is_empty() || !self.bool_result {
            if s.draft_version >= 2019 {
                self.refs_validate();
            }
            self.cond_validate();
            if s.draft_version >= 2019 {
                self.uneval_validate();
            }
        }

        if self.budget.spent() {
            return Err(self.spent_error());
        }
        match self.errors.len() {
            0 => Ok(self.uneval),
            1 => Err(self.errors.remove(0)),
            _ => {
                let mut e = self.error(kind!(Group));
                if !self.bool_result {
                    e.causes = self.errors;
                }
                Err(e)
            }
        }
    }
}

// type specific validations
impl<'v> Validator<'v, '_, '_, '_> {
    fn obj_validate(&mut self, obj: &'v Map<String, Value>) {
        let s = self.schema;
        // VIA patch: the names the dependency keywords look up; the
        // required names are charged as `find_missing` scans them.
        let names: u64 = s
            .dependencies
            .iter()
            .map(|(name, _)| name)
            .chain(s.dependent_schemas.iter().map(|(name, _)| name))
            .chain(s.dependent_required.iter().map(|(name, _)| name))
            .map(|name| pattern_units(name))
            .sum();
        if !self.budget.charge(names) {
            return;
        }
        macro_rules! add_err {
            ($result:expr) => {
                if let Err(e) = $result {
                    self.errors.push(e);
                }
            };
        }

        // minProperties --
        if let Some(min) = s.min_properties {
            if obj.len() < min {
                self.add_error(kind!(MinProperties, obj.len(), min));
            }
        }

        // maxProperties --
        if let Some(max) = s.max_properties {
            if obj.len() > max {
                self.add_error(kind!(MaxProperties, obj.len(), max));
            }
        }

        // required --
        if !s.required.is_empty() {
            if let Some(missing) = self.find_missing(obj, &s.required) {
                self.add_error(kind!(Required, want: missing));
            }
        }

        if self.bool_result && !self.errors.is_empty() {
            return;
        }

        // dependencies --
        for (prop, dep) in &s.dependencies {
            if self.budget.spent() {
                return;
            }
            if obj.contains_key(prop) {
                match dep {
                    Dependency::Props(required) => {
                        if let Some(missing) = self.find_missing(obj, required) {
                            self.add_error(ErrorKind::Dependency { prop, missing });
                        }
                    }
                    Dependency::SchemaRef(sch) => {
                        add_err!(self.validate_self(*sch));
                    }
                }
            }
        }

        let mut additional_props = vec![];
        for (pname, pvalue) in obj {
            if (self.bool_result && !self.errors.is_empty()) || self.budget.spent() {
                return;
            }
            let mut evaluated = false;

            // properties --
            if let Some(sch) = s.properties.get(pname) {
                evaluated = true;
                add_err!(self.validate_val(*sch, pvalue, prop!(pname)));
            }

            // patternProperties --
            for (regex, sch) in &s.pattern_properties {
                if !self.budget.charge(regex.units(pname)) {
                    return;
                }
                if regex.is_match(pname) {
                    evaluated = true;
                    add_err!(self.validate_val(*sch, pvalue, prop!(pname)));
                }
            }

            if !evaluated {
                // additionalProperties --
                if let Some(additional) = &s.additional_properties {
                    evaluated = true;
                    match additional {
                        Additional::Bool(allowed) => {
                            if !allowed {
                                additional_props.push(pname.into());
                            }
                        }
                        Additional::SchemaRef(sch) => {
                            add_err!(self.validate_val(*sch, pvalue, prop!(pname)));
                        }
                    }
                }
            }

            if evaluated {
                self.uneval.props.remove(pname);
            }
        }
        if !additional_props.is_empty() {
            self.add_error(kind!(AdditionalProperties, got: additional_props));
        }

        if s.draft_version == 4 {
            return;
        }

        // propertyNames --
        if let Some(sch) = &s.property_names {
            for pname in obj.keys() {
                if self.budget.spent() {
                    return;
                }
                // The name is copied: its units were charged with the
                // object's members.
                let v = Value::String(pname.to_owned());
                if let Err(mut e) = self.validate_nested(&v, *sch) {
                    e.schema_url = &s.loc;
                    e.kind = ErrorKind::PropertyName {
                        prop: pname.to_owned(),
                    };
                    self.errors.push(e.clone_static());
                }
            }
        }

        if s.draft_version == 6 {
            return;
        }

        // dependentSchemas --
        for (pname, sch) in &s.dependent_schemas {
            if self.budget.spent() {
                return;
            }
            if obj.contains_key(pname) {
                add_err!(self.validate_self(*sch));
            }
        }

        // dependentRequired --
        for (prop, required) in &s.dependent_required {
            if self.budget.spent() {
                return;
            }
            if obj.contains_key(prop) {
                if let Some(missing) = self.find_missing(obj, required) {
                    self.add_error(ErrorKind::DependentRequired { prop, missing });
                }
            }
        }
    }

    fn arr_validate(&mut self, arr: &'v Vec<Value>) {
        let s = self.schema;
        let len = arr.len();
        macro_rules! add_err {
            ($result:expr) => {
                if let Err(e) = $result {
                    self.errors.push(e);
                }
            };
        }

        // minItems --
        if let Some(min) = s.min_items {
            if len < min {
                self.add_error(kind!(MinItems, len, min));
            }
        }

        // maxItems --
        if let Some(max) = s.max_items {
            if len > max {
                self.add_error(kind!(MaxItems, len, max));
            }
        }

        // uniqueItems --
        if len > 1 && s.unique_items && charge_nodes(self.v, self.budget) {
            if let Some((i, j)) = duplicates(arr, self.budget) {
                self.add_error(kind!(UniqueItems, got: [i, j]));
            }
        }

        if s.draft_version < 2020 {
            let mut evaluated = 0;

            // items --
            if let Some(items) = &s.items {
                match items {
                    Items::SchemaRef(sch) => {
                        for (i, item) in arr.iter().enumerate() {
                            if self.budget.spent() {
                                return;
                            }
                            add_err!(self.validate_val(*sch, item, item!(i)));
                        }
                        evaluated = len;
                        debug_assert!(self.uneval.items.is_empty());
                    }
                    Items::SchemaRefs(list) => {
                        for (i, (item, sch)) in arr.iter().zip(list).enumerate() {
                            if self.budget.spent() {
                                return;
                            }
                            add_err!(self.validate_val(*sch, item, item!(i)));
                        }
                        evaluated = min(list.len(), len);
                    }
                }
            }

            // additionalItems --
            if let Some(additional) = &s.additional_items {
                match additional {
                    Additional::Bool(allowed) => {
                        if !allowed && evaluated != len {
                            self.add_error(kind!(AdditionalItems, got: len - evaluated));
                        }
                    }
                    Additional::SchemaRef(sch) => {
                        for (i, item) in arr[evaluated..].iter().enumerate() {
                            if self.budget.spent() {
                                return;
                            }
                            add_err!(self.validate_val(*sch, item, item!(i)));
                        }
                    }
                }
                debug_assert!(self.uneval.items.is_empty());
            }
        } else {
            // prefixItems --
            for (i, (sch, item)) in s.prefix_items.iter().zip(arr).enumerate() {
                if self.budget.spent() {
                    return;
                }
                add_err!(self.validate_val(*sch, item, item!(i)));
            }

            // items2020 --
            if let Some(sch) = &s.items2020 {
                let evaluated = min(s.prefix_items.len(), len);
                for (i, item) in arr[evaluated..].iter().enumerate() {
                    if self.budget.spent() {
                        return;
                    }
                    add_err!(self.validate_val(*sch, item, item!(i)));
                }
                debug_assert!(self.uneval.items.is_empty());
            }
        }

        // contains --
        if let Some(sch) = &s.contains {
            let mut matched = vec![];
            let mut errors = vec![];

            for (i, item) in arr.iter().enumerate() {
                if self.budget.spent() {
                    return;
                }
                if let Err(e) = self.validate_val(*sch, item, item!(i)) {
                    errors.push(e);
                } else {
                    matched.push(i);
                    if s.draft_version >= 2020 {
                        self.uneval.items.remove(&i);
                    }
                }
            }

            // minContains --
            if let Some(min) = s.min_contains {
                if matched.len() < min {
                    let mut e = self.error(kind!(MinContains, matched.clone(), min));
                    if !self.bool_result {
                        e.causes = errors;
                    }
                    self.errors.push(e);
                }
            } else if matched.is_empty() {
                let mut e = self.error(kind!(Contains));
                if !self.bool_result {
                    e.causes = errors;
                }
                self.errors.push(e);
            }

            // maxContains --
            if let Some(max) = s.max_contains {
                if matched.len() > max {
                    self.add_error(kind!(MaxContains, matched, max));
                }
            }
        }
    }

    fn str_validate(&mut self, str: &'v String) {
        let s = self.schema;
        let mut len = None;
        // VIA patch: counting characters scans the string.
        if (s.min_length.is_some() || s.max_length.is_some())
            && !self.budget.charge(pattern_units(str))
        {
            return;
        }

        // minLength --
        if let Some(min) = s.min_length {
            let len = len.get_or_insert_with(|| str.chars().count());
            if *len < min {
                self.add_error(kind!(MinLength, *len, min));
            }
        }

        // maxLength --
        if let Some(max) = s.max_length {
            let len = len.get_or_insert_with(|| str.chars().count());
            if *len > max {
                self.add_error(kind!(MaxLength, *len, max));
            }
        }

        // pattern --
        if let Some(regex) = &s.pattern {
            if !self.budget.charge(regex.units(str)) {
                return;
            }
            if !regex.is_match(str) {
                self.add_error(kind!(Pattern, str.into(), regex.as_str()));
            }
        }

        if s.draft_version == 6 {
            return;
        }

        // contentEncoding --
        let mut decoded = Some(Cow::from(str.as_bytes()));
        if let Some(decoder) = &s.content_encoding {
            match (decoder.func)(str) {
                Ok(bytes) => decoded = Some(Cow::from(bytes)),
                Err(err) => {
                    decoded = None;
                    self.add_error(ErrorKind::ContentEncoding {
                        want: decoder.name,
                        err,
                    })
                }
            }
        }

        // contentMediaType --
        let mut deserialized = None;
        if let (Some(mt), Some(decoded)) = (&s.content_media_type, decoded) {
            match (mt.func)(decoded.as_ref(), s.content_schema.is_some()) {
                Ok(des) => deserialized = des,
                Err(e) => {
                    self.add_error(kind!(ContentMediaType, decoded.into(), mt.name, e));
                }
            }
        }

        // contentSchema --
        if let (Some(sch), Some(v)) = (s.content_schema, deserialized) {
            if let Err(mut e) = self.validate_nested(&v, sch) {
                e.schema_url = &s.loc;
                e.kind = kind!(ContentSchema);
                self.errors.push(e.clone_static());
            }
        }
    }

    fn num_validate(&mut self, num: &'v Number) {
        let s = self.schema;

        // minimum --
        if let Some(min) = &s.minimum {
            if let (Some(minf), Some(numf)) = (min.as_f64(), num.as_f64()) {
                if numf < minf {
                    self.add_error(kind!(Minimum, Cow::Borrowed(num), min));
                }
            }
        }

        // maximum --
        if let Some(max) = &s.maximum {
            if let (Some(maxf), Some(numf)) = (max.as_f64(), num.as_f64()) {
                if numf > maxf {
                    self.add_error(kind!(Maximum, Cow::Borrowed(num), max));
                }
            }
        }

        // exclusiveMinimum --
        if let Some(ex_min) = &s.exclusive_minimum {
            if let (Some(ex_minf), Some(numf)) = (ex_min.as_f64(), num.as_f64()) {
                if numf <= ex_minf {
                    self.add_error(kind!(ExclusiveMinimum, Cow::Borrowed(num), ex_min));
                }
            }
        }

        // exclusiveMaximum --
        if let Some(ex_max) = &s.exclusive_maximum {
            if let (Some(ex_maxf), Some(numf)) = (ex_max.as_f64(), num.as_f64()) {
                if numf >= ex_maxf {
                    self.add_error(kind!(ExclusiveMaximum, Cow::Borrowed(num), ex_max));
                }
            }
        }

        // multipleOf --
        if let Some(mul) = &s.multiple_of {
            if let (Some(mulf), Some(numf)) = (mul.as_f64(), num.as_f64()) {
                if (numf / mulf).fract() != 0.0 {
                    self.add_error(kind!(MultipleOf, Cow::Borrowed(num), mul));
                }
            }
        }
    }
}

// references validation
impl<'v, 's> Validator<'v, 's, '_, '_> {
    fn refs_validate(&mut self) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                if let Err(e) = $result {
                    self.errors.push(e);
                }
            };
        }

        // $recursiveRef --
        if let Some(mut sch) = s.recursive_ref {
            if self.schemas.get(sch).recursive_anchor {
                sch = self.resolve_recursive_anchor(sch);
            }
            add_err!(self.validate_ref(sch, "$recursiveRef"));
        }

        // $dynamicRef --
        if let Some(dref) = &s.dynamic_ref {
            let mut sch = dref.sch; // initial target
            if let Some(anchor) = &dref.anchor {
                // VIA patch: comparing the anchor name is charged by its
                // length, before it runs.
                if !self.budget.charge(pattern_units(anchor)) {
                    return;
                }
                // $dynamicRef includes anchor
                if self.schemas.get(sch).dynamic_anchor == dref.anchor {
                    // initial target has matching $dynamicAnchor
                    sch = self.resolve_dynamic_anchor(anchor, sch);
                    if self.budget.spent() {
                        return;
                    }
                }
            }
            add_err!(self.validate_ref(sch, "$dynamicRef"));
        }
    }

    fn validate_ref(
        &mut self,
        sch: SchemaIndex,
        kw: &'static str,
    ) -> Result<(), ValidationError<'s, 'v>> {
        if let Err(err) = self._validate_self(sch, kw.into(), false) {
            let url = &self.schemas.get(sch).loc;
            let mut ref_err = self.error(ErrorKind::Reference { kw, url });
            if self.bool_result {
                // VIA patch: a yes/no answer keeps no error tree.
            } else if let ErrorKind::Group = err.kind {
                ref_err.causes = err.causes;
            } else {
                ref_err.causes.push(err);
            }
            return Err(ref_err);
        }
        Ok(())
    }

    fn resolve_recursive_anchor(&self, fallback: SchemaIndex) -> SchemaIndex {
        // VIA patch: the walk over the scope chain is charged.
        if !self.budget.charge(self.scope.depth as u64 + 1) {
            return fallback;
        }
        let mut sch = fallback;
        let mut scope = &self.scope;
        loop {
            let scope_sch = self.schemas.get(scope.sch);
            let base_sch = self.schemas.get(scope_sch.resource);
            if base_sch.recursive_anchor {
                sch = scope.sch
            }
            if let Some(parent) = scope.parent {
                scope = parent;
            } else {
                return sch;
            }
        }
    }

    fn resolve_dynamic_anchor(&self, name: &String, fallback: SchemaIndex) -> SchemaIndex {
        // VIA patch: the walk over the scope chain is charged, each scope's
        // lookup by the length of the name it hashes and compares, before
        // the walk runs.
        let scopes = self.scope.depth as u64 + 1;
        if !self.budget.charge(scopes.saturating_mul(pattern_units(name))) {
            return fallback;
        }
        let mut sch = fallback;
        let mut scope = &self.scope;
        loop {
            let scope_sch = self.schemas.get(scope.sch);
            let base_sch = self.schemas.get(scope_sch.resource);
            debug_assert_eq!(base_sch.idx, base_sch.resource);
            if let Some(dsch) = base_sch.dynamic_anchors.get(name) {
                sch = *dsch
            }
            if let Some(parent) = scope.parent {
                scope = parent;
            } else {
                return sch;
            }
        }
    }
}

// conditional validation
impl Validator<'_, '_, '_, '_> {
    fn cond_validate(&mut self) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                if let Err(e) = $result {
                    self.errors.push(e);
                }
            };
        }

        // not --
        if let Some(not) = s.not {
            if self._validate_self(not, None, true).is_ok() {
                self.add_error(kind!(Not));
            }
        }

        // allOf --
        if !s.all_of.is_empty() {
            let mut errors = vec![];
            for sch in &s.all_of {
                if self.budget.spent() {
                    return;
                }
                if let Err(e) = self.validate_self(*sch) {
                    errors.push(e);
                    if self.bool_result {
                        break;
                    }
                }
            }
            if !errors.is_empty() {
                self.add_errors(errors, kind!(AllOf));
            }
        }

        // anyOf --
        if !s.any_of.is_empty() {
            let mut matched = false;
            let mut errors = vec![];
            for sch in &s.any_of {
                if self.budget.spent() {
                    return;
                }
                match self.validate_self(*sch) {
                    Ok(_) => {
                        matched = true;
                        // for uneval, all schemas must be checked
                        if self.uneval.is_empty() {
                            break;
                        }
                    }
                    Err(e) => errors.push(e),
                }
            }
            if !matched {
                self.add_errors(errors, kind!(AnyOf));
            }
        }

        // oneOf --
        if !s.one_of.is_empty() {
            let mut matched = None;
            let mut errors = vec![];
            for (i, sch) in s.one_of.iter().enumerate() {
                if self.budget.spent() {
                    return;
                }
                if let Err(e) = self._validate_self(*sch, None, matched.is_some()) {
                    if matched.is_none() {
                        errors.push(e);
                    }
                } else {
                    match matched {
                        None => _ = matched.replace(i),
                        Some(prev) => {
                            self.add_error(ErrorKind::OneOf(Some((prev, i))));
                            break;
                        }
                    }
                }
            }
            if matched.is_none() {
                self.add_errors(errors, ErrorKind::OneOf(None));
            }
        }

        // if, then, else --
        if let Some(if_) = s.if_ {
            if self._validate_self(if_, None, true).is_ok() {
                if let Some(then) = s.then {
                    add_err!(self.validate_self(then));
                }
            } else if let Some(else_) = s.else_ {
                add_err!(self.validate_self(else_));
            }
        }
    }
}

// uneval validation
impl Validator<'_, '_, '_, '_> {
    fn uneval_validate(&mut self) {
        let s = self.schema;
        let v = self.v;
        macro_rules! add_err {
            ($result:expr) => {
                if let Err(e) = $result {
                    self.errors.push(e);
                }
            };
        }

        // unevaluatedProperties --
        if let (Some(sch), Value::Object(obj)) = (s.unevaluated_properties, v) {
            let uneval = std::mem::take(&mut self.uneval);
            for pname in &uneval.props {
                if self.budget.spent() {
                    return;
                }
                if let Some(pvalue) = obj.get(*pname) {
                    add_err!(self.validate_val(sch, pvalue, prop!(pname)));
                }
            }
            self.uneval.props.clear();
        }

        // unevaluatedItems --
        if let (Some(sch), Value::Array(arr)) = (s.unevaluated_items, v) {
            let uneval = std::mem::take(&mut self.uneval);
            for i in &uneval.items {
                if self.budget.spent() {
                    return;
                }
                if let Some(pvalue) = arr.get(*i) {
                    add_err!(self.validate_val(sch, pvalue, item!(*i)));
                }
            }
            self.uneval.items.clear();
        }
    }
}

// validation helpers
impl<'v, 's> Validator<'v, 's, '_, '_> {
    fn validate_val(
        &mut self,
        sch: SchemaIndex,
        v: &'v Value,
        token: InstanceToken<'v>,
    ) -> Result<(), ValidationError<'s, 'v>> {
        if self.vloc.len() == self.scope.vid {
            self.vloc.push(token);
        } else {
            self.vloc[self.scope.vid] = token;
        }
        let scope = self.scope.child(sch, None, self.scope.vid + 1);
        let schema = &self.schemas.get(sch);
        Validator {
            v,
            vloc: self.vloc,
            schema,
            schemas: self.schemas,
            scope,
            uneval: Uneval::from(v, schema, false),
            errors: vec![],
            bool_result: self.bool_result,
            budget: self.budget,
        }
        .validate()
        .map(|_| ())
    }

    fn _validate_self(
        &mut self,
        sch: SchemaIndex,
        ref_kw: Option<&'static str>,
        bool_result: bool,
    ) -> Result<(), ValidationError<'s, 'v>> {
        let scope = self.scope.child(sch, ref_kw, self.scope.vid);
        let schema = &self.schemas.get(sch);
        let result = Validator {
            v: self.v,
            vloc: self.vloc,
            schema,
            schemas: self.schemas,
            scope,
            uneval: Uneval::from(self.v, schema, !self.uneval.is_empty()),
            errors: vec![],
            bool_result: self.bool_result || bool_result,
            budget: self.budget,
        }
        .validate();
        if let Ok(reply) = &result {
            self.uneval.merge(reply);
        }
        result.map(|_| ())
    }

    #[inline(always)]
    fn validate_self(&mut self, sch: SchemaIndex) -> Result<(), ValidationError<'s, 'v>> {
        self._validate_self(sch, None, false)
    }

    // VIA patch: a value built during validation (`propertyNames`,
    // `contentSchema`) is validated within the same budget.
    fn validate_nested<'n>(
        &self,
        v: &'n Value,
        sch: SchemaIndex,
    ) -> Result<(), ValidationError<'s, 'n>> {
        let schema = self.schemas.get(sch);
        validate_within(v, schema, self.schemas, self.budget, self.bool_result)
    }
}

// error helpers
impl<'v, 's> Validator<'v, 's, '_, '_> {
    #[inline(always)]
    fn error(&self, kind: ErrorKind<'s, 'v>) -> ValidationError<'s, 'v> {
        if self.bool_result || self.budget.spent() {
            return ValidationError {
                schema_url: &self.schema.loc,
                instance_location: InstanceLocation::new(),
                kind: ErrorKind::Group,
                causes: vec![],
            };
        }
        ValidationError {
            schema_url: &self.schema.loc,
            instance_location: self.instance_location(),
            kind,
            causes: vec![],
        }
    }

    // VIA patch: the error of an evaluation the spent budget ended; it
    // allocates nothing.
    fn spent_error(&self) -> ValidationError<'s, 'v> {
        ValidationError {
            schema_url: &self.schema.loc,
            instance_location: InstanceLocation::new(),
            kind: ErrorKind::Group,
            causes: vec![],
        }
    }

    #[inline(always)]
    fn add_error(&mut self, kind: ErrorKind<'s, 'v>) {
        self.errors.push(self.error(kind));
    }

    #[inline(always)]
    fn add_errors(&mut self, errors: Vec<ValidationError<'s, 'v>>, kind: ErrorKind<'s, 'v>) {
        if errors.len() == 1 {
            self.errors.extend(errors);
        } else {
            let mut err = self.error(kind);
            if !self.bool_result {
                err.causes = errors;
            }
            self.errors.push(err);
        }
    }

    fn kw_loc(&self, mut scope: &Scope) -> String {
        let mut loc = String::new();
        while let Some(parent) = scope.parent {
            if let Some(kw) = scope.ref_kw {
                loc.insert_str(0, kw);
                loc.insert(0, '/');
            } else {
                let cur = &self.schemas.get(scope.sch).loc;
                let parent = &self.schemas.get(parent.sch).loc;
                loc.insert_str(0, &cur[parent.len()..]);
            }
            scope = parent;
        }
        loc
    }

    fn find_missing(
        &self,
        obj: &'v Map<String, Value>,
        required: &'s [String],
    ) -> Option<Vec<&'s str>> {
        // VIA patch: each name scanned is charged; a spent budget ends the
        // scan, and the evaluation with it.
        let budget = self.budget;
        let mut missing = required
            .iter()
            .take_while(|p| budget.charge(pattern_units(p)))
            .filter(|p| !obj.contains_key(p.as_str()))
            .map(|p| p.as_str());
        if self.bool_result {
            missing.next().map(|_| Vec::new())
        } else {
            let missing = missing.collect::<Vec<_>>();
            if missing.is_empty() {
                None
            } else {
                Some(missing)
            }
        }
    }

    fn instance_location(&self) -> InstanceLocation<'v> {
        let len = self.scope.vid;
        let mut tokens = Vec::with_capacity(len);
        for tok in &self.vloc[..len] {
            tokens.push(tok.clone());
        }
        InstanceLocation { tokens }
    }
}

// Uneval --

#[derive(Default)]
struct Uneval<'v> {
    props: HashSet<&'v String>,
    items: HashSet<usize>,
}

impl<'v> Uneval<'v> {
    fn is_empty(&self) -> bool {
        self.props.is_empty() && self.items.is_empty()
    }

    fn from(v: &'v Value, sch: &Schema, caller_needs: bool) -> Self {
        let mut uneval = Self::default();
        match v {
            Value::Object(obj) => {
                if !sch.all_props_evaluated
                    && (caller_needs || sch.unevaluated_properties.is_some())
                {
                    uneval.props = obj.keys().collect();
                }
            }
            Value::Array(arr) => {
                if !sch.all_items_evaluated
                    && (caller_needs || sch.unevaluated_items.is_some())
                    && sch.num_items_evaluated < arr.len()
                {
                    uneval.items = (sch.num_items_evaluated..arr.len()).collect();
                }
            }
            _ => (),
        }
        uneval
    }

    fn merge(&mut self, other: &Uneval) {
        self.props.retain(|p| other.props.contains(p));
        self.items.retain(|i| other.items.contains(i));
    }
}

// Scope ---

#[derive(Debug)]
struct Scope<'a> {
    sch: SchemaIndex,
    // if None, compute from self.sch and self.parent.sh
    // not None only when there is jump i.e $ref, $XXXRef
    ref_kw: Option<&'static str>,
    /// unique id of value being validated
    // if two scope validate same value, they will have same vid
    vid: usize,
    /// VIA patch: how many scopes this chain has above this one, which
    /// the dynamic-anchor walks visit.
    depth: usize,
    /// VIA patch: how many ancestors in a row validate the same value.
    run: usize,
    parent: Option<&'a Scope<'a>>,
}

impl Scope<'_> {
    fn child<'x>(
        &'x self,
        sch: SchemaIndex,
        ref_kw: Option<&'static str>,
        vid: usize,
    ) -> Scope<'x> {
        Scope {
            sch,
            ref_kw,
            vid,
            depth: self.depth + 1,
            run: if vid == self.vid { self.run + 1 } else { 0 },
            parent: Some(self),
        }
    }

    fn check_cycle(&self) -> Option<&Scope> {
        let mut scope = self.parent;
        while let Some(scp) = scope {
            if scp.vid != self.vid {
                break;
            }
            if scp.sch == self.sch {
                return Some(scp);
            }
            scope = scp.parent;
        }
        None
    }
}

/// Token in InstanceLocation json-pointer.
#[derive(Debug, Clone)]
pub enum InstanceToken<'v> {
    /// Token for property.
    Prop(Cow<'v, str>),
    /// Token for array item.
    Item(usize),
}

impl From<String> for InstanceToken<'_> {
    fn from(prop: String) -> Self {
        InstanceToken::Prop(prop.into())
    }
}

impl<'v> From<&'v str> for InstanceToken<'v> {
    fn from(prop: &'v str) -> Self {
        InstanceToken::Prop(prop.into())
    }
}

impl From<usize> for InstanceToken<'_> {
    fn from(index: usize) -> Self {
        InstanceToken::Item(index)
    }
}

/// The location of the JSON value within the instance being validated
#[derive(Debug, Default)]
pub struct InstanceLocation<'v> {
    pub tokens: Vec<InstanceToken<'v>>,
}

impl InstanceLocation<'_> {
    fn new() -> Self {
        Self::default()
    }

    fn clone_static(self) -> InstanceLocation<'static> {
        let mut tokens = Vec::with_capacity(self.tokens.len());
        for tok in self.tokens {
            let tok = match tok {
                InstanceToken::Prop(p) => InstanceToken::Prop(p.into_owned().into()),
                InstanceToken::Item(i) => InstanceToken::Item(i),
            };
            tokens.push(tok);
        }
        InstanceLocation { tokens }
    }
}

impl Display for InstanceLocation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for tok in &self.tokens {
            f.write_char('/')?;
            match tok {
                InstanceToken::Prop(s) => f.write_str(&escape(s))?,
                InstanceToken::Item(i) => write!(f, "{i}")?,
            }
        }
        Ok(())
    }
}

impl<'s> ValidationError<'s, '_> {
    pub(crate) fn clone_static(self) -> ValidationError<'s, 'static> {
        let mut causes = Vec::with_capacity(self.causes.len());
        for cause in self.causes {
            causes.push(cause.clone_static());
        }
        ValidationError {
            instance_location: self.instance_location.clone_static(),
            kind: self.kind.clone_static(),
            causes,
            ..self
        }
    }
}

impl<'s> ErrorKind<'s, '_> {
    fn clone_static(self) -> ErrorKind<'s, 'static> {
        use ErrorKind::*;
        match self {
            AdditionalProperties { got } => AdditionalProperties {
                got: got.into_iter().map(|e| e.into_owned().into()).collect(),
            },
            Format { got, want, err } => Format {
                got: Cow::Owned(got.into_owned()),
                want,
                err,
            },
            Pattern { got, want } => Pattern {
                got: got.into_owned().into(),
                want,
            },
            Minimum { got, want } => Minimum {
                got: Cow::Owned(got.into_owned()),
                want,
            },
            Maximum { got, want } => Maximum {
                got: Cow::Owned(got.into_owned()),
                want,
            },
            ExclusiveMinimum { got, want } => ExclusiveMinimum {
                got: Cow::Owned(got.into_owned()),
                want,
            },
            ExclusiveMaximum { got, want } => ExclusiveMaximum {
                got: Cow::Owned(got.into_owned()),
                want,
            },
            MultipleOf { got, want } => MultipleOf {
                got: Cow::Owned(got.into_owned()),
                want,
            },
            // #[cfg(not(debug_assertions))]
            // _ => unsafe { std::mem::transmute(self) },
            Group => Group,
            Schema { url } => Schema { url },
            ContentSchema => ContentSchema,
            PropertyName { prop } => PropertyName { prop },
            Reference { kw, url } => Reference { kw, url },
            RefCycle {
                url,
                kw_loc1,
                kw_loc2,
            } => RefCycle {
                url,
                kw_loc1,
                kw_loc2,
            },
            FalseSchema => FalseSchema,
            Type { got, want } => Type { got, want },
            Enum { want } => Enum { want },
            Const { want } => Const { want },
            MinProperties { got, want } => MinProperties { got, want },
            MaxProperties { got, want } => MaxProperties { got, want },
            Required { want } => Required { want },
            Dependency { prop, missing } => Dependency { prop, missing },
            DependentRequired { prop, missing } => DependentRequired { prop, missing },
            MinItems { got, want } => MinItems { got, want },
            MaxItems { got, want } => MaxItems { got, want },
            Contains => Contains,
            MinContains { got, want } => MinContains { got, want },
            MaxContains { got, want } => MaxContains { got, want },
            UniqueItems { got } => UniqueItems { got },
            AdditionalItems { got } => AdditionalItems { got },
            MinLength { got, want } => MinLength { got, want },
            MaxLength { got, want } => MaxLength { got, want },
            ContentEncoding { want, err } => ContentEncoding { want, err },
            ContentMediaType { got, want, err } => ContentMediaType { got, want, err },
            Not => Not,
            AllOf => AllOf,
            AnyOf => AnyOf,
            OneOf(opt) => OneOf(opt),
        }
    }
}
