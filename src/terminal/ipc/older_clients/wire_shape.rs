//! Traces the postcard shape of a type graph by deserializing it.
//!
//! The tracer is a [`serde::Deserializer`] that invents a value for every
//! request and records what was asked for. Postcard is not self-describing, so
//! the requests a type makes of its deserializer are exactly what it decodes.
//! Containers are keyed by their Rust type name, which tells apart the many
//! types here that share a short name. Each pass visits the next unvisited
//! variant of every enum it meets, and otherwise steers toward a container
//! that is still incomplete; passes repeat until nothing new is found. A
//! string some type validates, such as a color or URL, is retried at its
//! position with the next of a few [`STRINGS`].

use std::{
    any::type_name,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{
    Deserialize, Serialize,
    de::{
        self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess,
        Visitor,
    },
};

/// The shape of one value position.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Shape {
    /// Never reached by tracing.
    #[default]
    Unknown,
    Unit,
    Bool,
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    F32,
    F64,
    Char,
    Str,
    Bytes,
    Option(Box<Shape>),
    Seq(Box<Shape>),
    Map(Box<Shape>, Box<Shape>),
    Tuple(Vec<Shape>),
    /// A struct or enum, by its key in [`Shapes::containers`].
    Container(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Container {
    UnitStruct,
    Newtype(Shape),
    TupleStruct(Vec<Shape>),
    Struct(Vec<(String, Shape)>),
    /// Variants by postcard index; `None` until a pass visits it.
    Enum(Vec<(String, Option<Fields>)>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Fields {
    Unit,
    Newtype(Shape),
    Tuple(Vec<Shape>),
    Struct(Vec<(String, Shape)>),
}

/// The traced shapes of a protocol's client and server envelopes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Shapes {
    pub(super) client: Shape,
    pub(super) server: Shape,
    pub(super) containers: BTreeMap<String, Container>,
}

impl Shapes {
    /// Traces `Client` and `Server` until every reachable variant is visited.
    pub(super) fn trace<Client, Server>() -> std::result::Result<Self, String>
    where
        Client: for<'de> Deserialize<'de>,
        Server: for<'de> Deserialize<'de>,
    {
        let mut tracer = Tracer::default();
        let mut client = Shape::Unknown;
        let mut server = Shape::Unknown;
        let mut errors = Vec::new();
        for _ in 0..10_000 {
            tracer.progressed = false;
            tracer.unfinished = unfinished(&tracer.containers);
            tracer.decisions.clear();
            errors.clear();
            tracer.position = "client".into();
            tracer.last_string = None;
            if let Err(error) = Client::deserialize(Trace::new(&mut tracer, &mut client)) {
                errors.push(format!("client: {error}"));
                tracer.retry_string(&error);
            }
            tracer.position = "server".into();
            tracer.last_string = None;
            if let Err(error) = Server::deserialize(Trace::new(&mut tracer, &mut server)) {
                errors.push(format!("server: {error}"));
                tracer.retry_string(&error);
            }
            if !tracer.progressed {
                break;
            }
        }
        let shapes = Self {
            client,
            server,
            containers: tracer.containers,
        };
        let untraced = shapes.untraced();
        if untraced.is_empty() {
            Ok(shapes)
        } else {
            Err(format!(
                "untraced: {}\nlast errors: {}\nlast pass took: {}",
                untraced.join(", "),
                errors.join("; "),
                tracer.decisions.join(" ")
            ))
        }
    }

    fn untraced(&self) -> Vec<String> {
        fn unknown(shape: &Shape) -> bool {
            match shape {
                Shape::Unknown => true,
                Shape::Option(inner) | Shape::Seq(inner) => unknown(inner),
                Shape::Map(key, value) => unknown(key) || unknown(value),
                Shape::Tuple(shapes) => shapes.iter().any(unknown),
                _ => false,
            }
        }
        fn positions<'s>(at: String, fields: &'s Fields, into: &mut Vec<(String, &'s Shape)>) {
            match fields {
                Fields::Unit => {}
                Fields::Newtype(shape) => into.push((at, shape)),
                Fields::Tuple(shapes) => into.extend(
                    shapes
                        .iter()
                        .enumerate()
                        .map(|(index, shape)| (format!("{at}.{index}"), shape)),
                ),
                Fields::Struct(named) => into.extend(
                    named
                        .iter()
                        .map(|(field, shape)| (format!("{at}.{field}"), shape)),
                ),
            }
        }
        let mut untraced = Vec::new();
        for (name, container) in &self.containers {
            let mut shapes = Vec::new();
            match container {
                Container::UnitStruct => {}
                Container::Newtype(shape) => shapes.push((name.clone(), shape)),
                Container::TupleStruct(each) => {
                    shapes.extend(
                        each.iter()
                            .enumerate()
                            .map(|(index, shape)| (format!("{name}.{index}"), shape)),
                    );
                }
                Container::Struct(named) => shapes.extend(
                    named
                        .iter()
                        .map(|(field, shape)| (format!("{name}.{field}"), shape)),
                ),
                Container::Enum(variants) => {
                    for (variant, fields) in variants {
                        match fields {
                            None => untraced.push(format!("{name}::{variant}")),
                            Some(fields) => {
                                positions(format!("{name}::{variant}"), fields, &mut shapes)
                            }
                        }
                    }
                }
            }
            untraced.extend(
                shapes
                    .into_iter()
                    .filter(|(_, shape)| unknown(shape))
                    .map(|(at, _)| at),
            );
        }
        untraced
    }
}

/// A value of `T` decoded from what the tracer invents, for checking how such
/// a value encodes.
pub(super) fn invent<T: for<'de> Deserialize<'de>>() -> std::result::Result<T, String> {
    let mut tracer = Tracer::default();
    let mut shape = Shape::Unknown;
    let mut error = String::new();
    for _ in 0..100 {
        tracer.progressed = false;
        tracer.unfinished = unfinished(&tracer.containers);
        tracer.position = "invented".into();
        tracer.last_string = None;
        match T::deserialize(Trace::new(&mut tracer, &mut shape)) {
            Ok(value) => return Ok(value),
            Err(failure) => {
                tracer.retry_string(&failure);
                error = failure.to_string();
            }
        }
        if !tracer.progressed {
            break;
        }
    }
    Err(error)
}

#[derive(Default)]
struct Tracer {
    containers: BTreeMap<String, Container>,
    /// The next variant to visit, per enum.
    next_variant: BTreeMap<String, usize>,
    /// Containers being traced, outermost first.
    stack: Vec<String>,
    /// Containers with an unvisited variant or an untraced position, as of
    /// the start of this pass.
    unfinished: BTreeSet<String>,
    /// Where the value being decoded sits: a container and element index.
    position: String,
    /// The candidate in [`STRINGS`] each position decodes.
    strings: BTreeMap<String, usize>,
    /// The position of the latest primitive, when it was a string.
    last_string: Option<String>,
    /// The variants the current pass took, for diagnosing a stall.
    decisions: Vec<String>,
    progressed: bool,
}

/// Strings that satisfy the validating types here, tried in order where a
/// pass fails right after decoding a string.
const STRINGS: &[&str] = &["", "#000000", "https://example.invalid/", "a", "0", "/"];

impl Tracer {
    /// A pass failed: when a string was the last value decoded, the next pass
    /// gives its position the next candidate.
    fn retry_string(&mut self, error: &TraceError) {
        let Some(position) = self.last_string.take() else {
            return;
        };
        if !error.refused {
            return;
        }
        let candidate = self.strings.entry(position).or_default();
        if *candidate + 1 < STRINGS.len() {
            *candidate += 1;
            self.progressed = true;
        }
    }

    fn string(&mut self) -> &'static str {
        let candidate = self
            .strings
            .get(&self.position)
            .copied()
            .unwrap_or_default();
        self.last_string = Some(self.position.clone());
        STRINGS[candidate]
    }

    /// The base position of the elements of the innermost container.
    fn within(&self, suffix: impl fmt::Display) -> String {
        format!("{}{suffix}", self.stack.last().map_or("", String::as_str))
    }

    fn on_stack(&self, shape: &Shape) -> bool {
        matches!(shape, Shape::Container(name) if self.stack.contains(name))
    }

    /// Whether decoding `shape` can reach unfinished work without passing
    /// through a container already being traced, where decoding is cut.
    fn reaches_unfinished(&self, shape: &Shape) -> bool {
        let mut seen: BTreeSet<&str> = self.stack.iter().map(String::as_str).collect();
        let mut pending = vec![shape];
        while let Some(shape) = pending.pop() {
            match shape {
                Shape::Unknown => return true,
                Shape::Container(name) => {
                    if !seen.insert(name) {
                        continue;
                    }
                    if self.unfinished.contains(name) {
                        return true;
                    }
                    if let Some(container) = self.containers.get(name) {
                        pending.extend(container.shapes());
                    }
                }
                Shape::Option(inner) | Shape::Seq(inner) => pending.push(inner),
                Shape::Map(key, value) => pending.extend([&**key, &**value]),
                Shape::Tuple(shapes) => pending.extend(shapes),
                _ => {}
            }
        }
        false
    }

    /// Whether decoding `shape` necessarily re-enters a container being
    /// traced. Options, sequences and maps are cut there instead.
    fn recurses(&self, shape: &Shape) -> bool {
        match shape {
            Shape::Container(name) => self.stack.contains(name),
            Shape::Tuple(shapes) => shapes.iter().any(|shape| self.recurses(shape)),
            _ => false,
        }
    }

    /// The variant a pass takes once every variant has been visited: one
    /// leading to unfinished work, else one without a payload, else any that
    /// does not lead back into a type being traced.
    fn revisit(&self, variants: &[(String, Option<Fields>)]) -> usize {
        let fields = |index: usize| {
            variants[index]
                .1
                .as_ref()
                .map_or_else(Vec::new, Fields::shapes)
        };
        let safe = |index: usize| !fields(index).iter().any(|shape| self.recurses(shape));
        let indices = 0..variants.len();
        indices
            .clone()
            .find(|&index| {
                safe(index)
                    && fields(index)
                        .iter()
                        .any(|shape| self.reaches_unfinished(shape))
            })
            .or_else(|| {
                indices
                    .clone()
                    .find(|&index| matches!(variants[index].1, Some(Fields::Unit)))
            })
            .or_else(|| indices.clone().find(|&index| safe(index)))
            .unwrap_or(0)
    }
}

impl Fields {
    fn shapes(&self) -> Vec<&Shape> {
        match self {
            Self::Unit => Vec::new(),
            Self::Newtype(shape) => vec![shape],
            Self::Tuple(shapes) => shapes.iter().collect(),
            Self::Struct(named) => named.iter().map(|(_, shape)| shape).collect(),
        }
    }
}

#[derive(Debug)]
pub(super) struct TraceError {
    message: String,
    /// Raised by a type refusing a value it was given, rather than by the
    /// tracer cutting recursion or finding conflicting shapes.
    refused: bool,
}

impl TraceError {
    fn tracer(message: String) -> Self {
        Self {
            message,
            refused: false,
        }
    }
}

impl fmt::Display for TraceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TraceError {}

impl de::Error for TraceError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self {
            message: message.to_string(),
            refused: true,
        }
    }
}

type Result<T> = std::result::Result<T, TraceError>;

impl Container {
    /// The positions recorded for every visited variant or field.
    fn shapes(&self) -> Vec<&Shape> {
        match self {
            Self::UnitStruct => Vec::new(),
            Self::Newtype(shape) => vec![shape],
            Self::TupleStruct(shapes) => shapes.iter().collect(),
            Self::Struct(fields) => fields.iter().map(|(_, shape)| shape).collect(),
            Self::Enum(variants) => variants
                .iter()
                .filter_map(|(_, fields)| fields.as_ref())
                .flat_map(Fields::shapes)
                .collect(),
        }
    }
}

/// The containers with an unvisited variant or an untraced position.
fn unfinished(containers: &BTreeMap<String, Container>) -> BTreeSet<String> {
    fn unknown(shape: &Shape) -> bool {
        match shape {
            Shape::Unknown => true,
            Shape::Option(inner) | Shape::Seq(inner) => unknown(inner),
            Shape::Map(key, value) => unknown(key) || unknown(value),
            Shape::Tuple(shapes) => shapes.iter().any(unknown),
            _ => false,
        }
    }
    containers
        .iter()
        .filter(|(_, container)| {
            matches!(container, Container::Enum(variants)
                if variants.iter().any(|(_, fields)| fields.is_none()))
                || container.shapes().into_iter().any(unknown)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Fills `slot` while deserializing one value.
struct Trace<'t> {
    tracer: &'t mut Tracer,
    slot: &'t mut Shape,
}

impl<'t> Trace<'t> {
    fn new(tracer: &'t mut Tracer, slot: &'t mut Shape) -> Self {
        Self { tracer, slot }
    }

    fn record(&mut self, shape: Shape) -> Result<()> {
        match (&*self.slot, &shape) {
            (Shape::Unknown, _) => {
                *self.slot = shape;
                self.tracer.progressed = true;
                Ok(())
            }
            (Shape::Option(_), Shape::Option(_))
            | (Shape::Seq(_), Shape::Seq(_))
            | (Shape::Map(..), Shape::Map(..))
            | (Shape::Tuple(_), Shape::Tuple(_)) => Ok(()),
            (recorded, shape) if recorded == shape => Ok(()),
            (recorded, shape) => Err(TraceError::tracer(format!(
                "one position decodes as {recorded:?} and as {shape:?}"
            ))),
        }
    }

    /// Records a container at this position and traces it, unless it is
    /// already being traced further out: a recursive type is cut there.
    fn container<V, F>(mut self, name: String, initial: Container, trace: F) -> Result<V>
    where
        F: FnOnce(&mut Tracer, &mut Container) -> Result<V>,
    {
        self.record(Shape::Container(name.clone()))?;
        if self.tracer.stack.contains(&name) {
            return Err(TraceError::tracer(format!("{name} contains itself")));
        }
        let mut container = match self.tracer.containers.remove(&name) {
            Some(container) => container,
            None => {
                self.tracer.progressed = true;
                initial
            }
        };
        self.tracer.stack.push(name.clone());
        let result = trace(self.tracer, &mut container);
        self.tracer.stack.pop();
        self.tracer.containers.insert(name, container);
        result
    }

    fn fields<'de, V: Visitor<'de>>(
        tracer: &mut Tracer,
        base: String,
        fields: &mut Vec<Shape>,
        visitor: V,
    ) -> Result<V::Value> {
        Self::counted(tracer, base, fields, visitor).0
    }

    /// Decodes positional elements, and how many the visitor read.
    fn counted<'de, V: Visitor<'de>>(
        tracer: &mut Tracer,
        base: String,
        fields: &mut Vec<Shape>,
        visitor: V,
    ) -> (Result<V::Value>, usize) {
        let mut elements = Elements {
            tracer,
            base,
            shapes: fields,
            next: 0,
        };
        let result = visitor.visit_seq(&mut elements);
        (result, elements.next)
    }

    /// Decodes a struct's fields. Serde lists a field's aliases among the
    /// names it gives, so a struct with one reads fewer positions than it
    /// names; its positions are then named by index, which is all postcard
    /// encodes.
    fn named<'de, V: Visitor<'de>>(
        tracer: &mut Tracer,
        base: String,
        recorded: &mut Vec<(String, Shape)>,
        visitor: V,
    ) -> Result<V::Value> {
        let mut shapes: Vec<Shape> = recorded.iter().map(|(_, shape)| shape.clone()).collect();
        let (result, read) = Self::counted(tracer, base, &mut shapes, visitor);
        if result.is_ok() && read < shapes.len() {
            shapes.truncate(read);
            *recorded = shapes
                .into_iter()
                .enumerate()
                .map(|(index, shape)| (index.to_string(), shape))
                .collect();
            tracer.progressed = true;
        } else {
            for ((_, recorded), shape) in recorded.iter_mut().zip(shapes) {
                *recorded = shape;
            }
        }
        result
    }
}

fn field_names(fields: &[&str]) -> Vec<(String, Shape)> {
    fields
        .iter()
        .map(|name| ((*name).to_owned(), Shape::Unknown))
        .collect()
}

/// A fixed number of positional elements, as postcard encodes a struct or tuple.
struct Elements<'a> {
    tracer: &'a mut Tracer,
    base: String,
    shapes: &'a mut Vec<Shape>,
    next: usize,
}

impl<'de> SeqAccess<'de> for Elements<'_> {
    type Error = TraceError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        let Some(slot) = self.shapes.get_mut(self.next) else {
            return Ok(None);
        };
        self.tracer.position = format!("{}#{}", self.base, self.next);
        self.next += 1;
        seed.deserialize(Trace::new(self.tracer, slot)).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.shapes.len() - self.next)
    }
}

/// One element of a sequence or map, or none where the element type is
/// already being traced further out.
struct Repeated<'a> {
    tracer: &'a mut Tracer,
    key: Option<&'a mut Shape>,
    value: &'a mut Shape,
    remaining: usize,
}

impl<'de> SeqAccess<'de> for Repeated<'_> {
    type Error = TraceError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(Trace::new(self.tracer, self.value))
            .map(Some)
    }
}

impl<'de> MapAccess<'de> for Repeated<'_> {
    type Error = TraceError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let key = self.key.as_deref_mut().expect("a map key");
        seed.deserialize(Trace::new(self.tracer, key)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(Trace::new(self.tracer, self.value))
    }
}

struct Variant<'a> {
    tracer: &'a mut Tracer,
    index: usize,
    fields: &'a mut Option<Fields>,
}

impl<'de, 'a> EnumAccess<'de> for Variant<'a> {
    type Error = TraceError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self)> {
        let index = u32::try_from(self.index).map_err(de::Error::custom)?;
        let tag = seed.deserialize(index.into_deserializer())?;
        Ok((tag, self))
    }
}

impl<'de> VariantAccess<'de> for Variant<'_> {
    type Error = TraceError;

    fn unit_variant(self) -> Result<()> {
        if self.fields.is_none() {
            self.tracer.progressed = true;
        }
        *self.fields = Some(Fields::Unit);
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        let mut shape = match self.fields.take() {
            Some(Fields::Newtype(shape)) => shape,
            _ => {
                self.tracer.progressed = true;
                Shape::Unknown
            }
        };
        self.tracer.position = self.tracer.within(format_args!("::{}", self.index));
        let result = seed.deserialize(Trace::new(self.tracer, &mut shape));
        *self.fields = Some(Fields::Newtype(shape));
        result
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        let mut shapes = match self.fields.take() {
            Some(Fields::Tuple(shapes)) => shapes,
            _ => {
                self.tracer.progressed = true;
                vec![Shape::Unknown; len]
            }
        };
        let base = self.tracer.within(format_args!("::{}", self.index));
        let result = Trace::fields(self.tracer, base, &mut shapes, visitor);
        *self.fields = Some(Fields::Tuple(shapes));
        result
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let mut named = match self.fields.take() {
            Some(Fields::Struct(named)) => named,
            _ => {
                self.tracer.progressed = true;
                field_names(fields)
            }
        };
        let base = self.tracer.within(format_args!("::{}", self.index));
        let result = Trace::named(self.tracer, base, &mut named, visitor);
        *self.fields = Some(Fields::Struct(named));
        result
    }
}

macro_rules! primitive {
    ($($method:ident => $shape:ident, $visit:ident($value:expr);)*) => {
        $(fn $method<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
            self.record(Shape::$shape)?;
            self.tracer.last_string = None;
            visitor.$visit($value)
        })*
    };
}

impl<'de> de::Deserializer<'de> for Trace<'_> {
    type Error = TraceError;

    primitive! {
        deserialize_bool => Bool, visit_bool(false);
        deserialize_i8 => I8, visit_i8(0);
        deserialize_i16 => I16, visit_i16(0);
        deserialize_i32 => I32, visit_i32(0);
        deserialize_i64 => I64, visit_i64(0);
        deserialize_i128 => I128, visit_i128(0);
        deserialize_u8 => U8, visit_u8(0);
        deserialize_u16 => U16, visit_u16(0);
        deserialize_u32 => U32, visit_u32(0);
        deserialize_u64 => U64, visit_u64(0);
        deserialize_u128 => U128, visit_u128(0);
        deserialize_f32 => F32, visit_f32(0.0);
        deserialize_f64 => F64, visit_f64(0.0);
        deserialize_char => Char, visit_char('a');
        // Sixteen bytes decode as the UUID in every identifier.
        deserialize_bytes => Bytes, visit_bytes(&[1; 16]);
        deserialize_byte_buf => Bytes, visit_byte_buf(vec![1; 16]);
    }

    fn deserialize_str<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
        self.record(Shape::Str)?;
        visitor.visit_str(self.tracer.string())
    }

    fn deserialize_string<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
        self.record(Shape::Str)?;
        visitor.visit_string(self.tracer.string().to_owned())
    }

    fn deserialize_unit<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
        self.record(Shape::Unit)?;
        visitor.visit_unit()
    }

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value> {
        Err(TraceError::tracer(
            "postcard cannot decode a self-describing value".into(),
        ))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let mut inner = match std::mem::take(self.slot) {
            Shape::Option(inner) => *inner,
            Shape::Unknown => {
                self.tracer.progressed = true;
                Shape::Unknown
            }
            other => {
                *self.slot = other.clone();
                return Err(TraceError::tracer(format!(
                    "{other:?} decoded as an option"
                )));
            }
        };
        let result = if self.tracer.on_stack(&inner) {
            visitor.visit_none()
        } else {
            visitor.visit_some(Trace::new(self.tracer, &mut inner))
        };
        *self.slot = Shape::Option(Box::new(inner));
        result
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let mut inner = match std::mem::take(self.slot) {
            Shape::Seq(inner) => *inner,
            Shape::Unknown => {
                self.tracer.progressed = true;
                Shape::Unknown
            }
            other => {
                *self.slot = other.clone();
                return Err(TraceError::tracer(format!(
                    "{other:?} decoded as a sequence"
                )));
            }
        };
        let remaining = usize::from(!self.tracer.on_stack(&inner));
        let result = visitor.visit_seq(Repeated {
            tracer: self.tracer,
            key: None,
            value: &mut inner,
            remaining,
        });
        *self.slot = Shape::Seq(Box::new(inner));
        result
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (mut key, mut value) = match std::mem::take(self.slot) {
            Shape::Map(key, value) => (*key, *value),
            Shape::Unknown => {
                self.tracer.progressed = true;
                (Shape::Unknown, Shape::Unknown)
            }
            other => {
                *self.slot = other.clone();
                return Err(TraceError::tracer(format!("{other:?} decoded as a map")));
            }
        };
        let remaining = usize::from(!self.tracer.on_stack(&key) && !self.tracer.on_stack(&value));
        let result = visitor.visit_map(Repeated {
            tracer: self.tracer,
            key: Some(&mut key),
            value: &mut value,
            remaining,
        });
        *self.slot = Shape::Map(Box::new(key), Box::new(value));
        result
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        let mut shapes = match std::mem::take(self.slot) {
            Shape::Tuple(shapes) if shapes.len() == len => shapes,
            Shape::Unknown => {
                self.tracer.progressed = true;
                vec![Shape::Unknown; len]
            }
            other => {
                *self.slot = other.clone();
                return Err(TraceError::tracer(format!(
                    "{other:?} decoded as a {len}-tuple"
                )));
            }
        };
        let base = format!("{}/", self.tracer.position);
        let result = Trace::fields(self.tracer, base, &mut shapes, visitor);
        *self.slot = Shape::Tuple(shapes);
        result
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.container(
            type_name::<V::Value>().to_owned(),
            Container::UnitStruct,
            |_, _| visitor.visit_unit(),
        )
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.container(
            type_name::<V::Value>().to_owned(),
            Container::Newtype(Shape::Unknown),
            |tracer, container| match container {
                Container::Newtype(shape) => {
                    tracer.position = tracer.within("#0");
                    visitor.visit_newtype_struct(Trace::new(tracer, shape))
                }
                other => Err(TraceError::tracer(format!(
                    "{other:?} decoded as a newtype"
                ))),
            },
        )
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.container(
            type_name::<V::Value>().to_owned(),
            Container::TupleStruct(vec![Shape::Unknown; len]),
            |tracer, container| match container {
                Container::TupleStruct(shapes) => {
                    let base = tracer.within("");
                    Trace::fields(tracer, base, shapes, visitor)
                }
                other => Err(TraceError::tracer(format!(
                    "{other:?} decoded as a tuple struct"
                ))),
            },
        )
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.container(
            type_name::<V::Value>().to_owned(),
            Container::Struct(field_names(fields)),
            |tracer, container| match container {
                Container::Struct(named) => {
                    let base = tracer.within("");
                    Trace::named(tracer, base, named, visitor)
                }
                other => Err(TraceError::tracer(format!("{other:?} decoded as a struct"))),
            },
        )
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let name = type_name::<V::Value>().to_owned();
        let initial = Container::Enum(
            variants
                .iter()
                .map(|variant| ((*variant).to_owned(), None))
                .collect(),
        );
        let key = name.clone();
        self.container(name, initial, move |tracer, container| {
            let Container::Enum(recorded) = container else {
                return Err(TraceError::tracer(format!(
                    "{container:?} decoded as an enum"
                )));
            };
            if recorded.is_empty() {
                return Err(TraceError::tracer(format!("{key} has no variants")));
            }
            // Visit each variant once; afterwards prefer one without a payload,
            // which cannot lead back into a type being traced.
            let next = tracer.next_variant.entry(key.clone()).or_default();
            let index = if *next < recorded.len() {
                *next += 1;
                *next - 1
            } else {
                let index = tracer.revisit(recorded);
                let short = key.rsplit("::").next().unwrap_or(&key);
                tracer
                    .decisions
                    .push(format!("{short}::{}", recorded[index].0));
                index
            };
            visitor.visit_enum(Variant {
                tracer,
                index,
                fields: &mut recorded[index].1,
            })
        })
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum Inner {
        A,
        B(u8),
        C { label: String },
    }

    /// Leads only back into the enum that holds it.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Back {
        outer: Option<Box<Outer>>,
    }

    /// `Back` precedes `Holds`, so a pass that steers toward any container
    /// still incomplete overall would keep taking `Back` and never finish
    /// `Inner`.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum Outer {
        Empty,
        Back(Back),
        Holds(Inner),
        Many(Vec<Outer>),
    }

    /// A string type that refuses the tracer's first candidate.
    struct Color;

    impl<'de> Deserialize<'de> for Color {
        fn deserialize<D: de::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            let text = String::deserialize(deserializer)?;
            if text.starts_with('#') {
                Ok(Self)
            } else {
                Err(de::Error::custom("expected #RRGGBB color"))
            }
        }
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Top {
        color: Color,
        #[serde(alias = "nested")]
        outer: Outer,
        next: Option<Box<Top>>,
    }

    #[test]
    fn nested_enums_validated_strings_and_recursion_are_traced() {
        let shapes = Shapes::trace::<Top, Outer>().unwrap();
        let Some(Container::Enum(inner)) = shapes.containers.get(type_name::<Inner>()) else {
            panic!("{:?}", shapes.containers.keys().collect::<Vec<_>>());
        };
        assert_eq!(
            inner
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        assert_eq!(
            inner[2].1,
            Some(Fields::Struct(vec![("label".into(), Shape::Str)]))
        );
        let Some(Container::Struct(top)) = shapes.containers.get(type_name::<Top>()) else {
            panic!("Top was not traced");
        };
        // `nested` is an alias, so the three positions are named by index.
        assert_eq!(top.len(), 3);
        assert_eq!(top[0], ("0".into(), Shape::Str));
        assert_eq!(
            top[2],
            (
                "2".into(),
                Shape::Option(Box::new(Shape::Container(type_name::<Top>().into())))
            )
        );
    }
}
