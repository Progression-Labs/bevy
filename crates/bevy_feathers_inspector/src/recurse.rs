//! The reflection recursion engine.
//!
//! [`build_value`] is the inspector's analog of `bevy-inspector-egui`'s `ui_for_reflect`: given a
//! reflected value it first checks for a registered per-type widget ([`ReflectInspectorWidget`]),
//! and otherwise recurses structurally. Scalar fields become a labeled row; compound fields
//! (structs, enums, lists) become a titled feathers [`group`] "card".

use bevy_ecs::component::Component;
use bevy_ecs::hierarchy::Children;
use bevy_reflect::structs::Struct;
use bevy_reflect::tuple_struct::TupleStruct;
use bevy_reflect::{ParsedPath, PartialReflect, ReflectRef, TypeRegistry};
use bevy_scene::prelude::*;
use bevy_scene::Scene;
use bevy_ui::{percent, px, AlignItems, Display, FlexDirection, JustifyContent, Node};

use bevy_feathers::containers::{group, group_body, group_header};
use bevy_feathers::display::{label, label_dim};

use crate::attributes::FieldCtx;
use crate::binding::InspectorRoot;
use crate::collapse::{
    collapse_toggle, on_header_click, CollapseBody, CollapseGroup, InspectorCollapsed,
};
use crate::enums::build_enum;
use crate::lists::build_list;
use crate::widget::ReflectInspectorWidget;

/// Invariant context shared across one component/resource's recursion.
pub struct BuildCx<'a> {
    /// The type registry (for widget lookup and recursion).
    pub registry: &'a TypeRegistry,
    /// The reflected value this subtree edits.
    pub root: InspectorRoot,
    /// Which cards under this root are collapsed.
    pub collapsed: &'a InspectorCollapsed,
}

/// Build an editing scene for `value`, reachable from `cx.root` via the reflection path `path`.
pub fn build_value(
    cx: &BuildCx,
    path: &str,
    value: &dyn PartialReflect,
    field: &FieldCtx,
) -> Box<dyn Scene> {
    // Read-only fields display their value without an editing widget.
    if field.read_only {
        return Box::new(read_only_leaf(value));
    }

    // Per-type widget override (leaf).
    if let Some(type_id) = value
        .get_represented_type_info()
        .map(bevy_reflect::TypeInfo::type_id)
        && let Some(widget) = cx.registry.get_type_data::<ReflectInspectorWidget>(type_id)
    {
        return (widget.build)(cx, path, value, field);
    }

    // Structural recursion.
    match value.reflect_ref() {
        ReflectRef::Struct(strukt) => build_struct(cx, path, value, strukt),
        ReflectRef::TupleStruct(tuple_struct) => build_tuple_struct(cx, path, tuple_struct),
        ReflectRef::Enum(enum_ref) => build_enum(cx, path, value, enum_ref),
        ReflectRef::List(list) => build_list(cx, path, list),
        ReflectRef::Array(array) => {
            let rows = (0..array.len())
                .filter_map(|i| {
                    let child = array.get(i)?;
                    let child_path = format!("{path}[{i}]");
                    Some(field_entry(
                        cx,
                        &i.to_string(),
                        &child_path,
                        child,
                        &FieldCtx::default(),
                    ))
                })
                .collect();
            Box::new(column(rows))
        }
        _ => Box::new(read_only_leaf(value)),
    }
}

/// Recurse into a named struct, one entry per field.
fn build_struct(
    cx: &BuildCx,
    path: &str,
    value: &dyn PartialReflect,
    strukt: &dyn Struct,
) -> Box<dyn Scene> {
    let info = value
        .get_represented_type_info()
        .and_then(|i| i.as_struct().ok());
    // The struct's own rows and its groups ([`Group`]), each where its first field stands.
    let mut body = Grouped::default();
    for i in 0..strukt.field_len() {
        let (Some(name), Some(child)) = (strukt.name_at(i), strukt.field_at(i)) else {
            continue;
        };
        let field_info = info.and_then(|si| si.field_at(i));
        if field_info.is_some_and(FieldCtx::is_hidden) {
            continue;
        }
        let child_field = field_info.map(FieldCtx::from_field).unwrap_or_default();
        let child_path = format!("{path}.{name}");
        let entry = field_entry(cx, name, &child_path, child, &child_field);
        let group = field_info.map(FieldCtx::group_of).unwrap_or_default();
        body.at(&group).items.push(Item::Entry(entry));
    }
    Box::new(column(body.scenes(cx, path, "")))
}

/// A struct's body as it is laid out: rows and group cards in the order they first appear.
#[derive(Default)]
struct Grouped {
    items: Vec<Item>,
}

enum Item {
    Entry(Box<dyn Scene>),
    Group(&'static str, Grouped),
}

impl Grouped {
    /// The group at `group` below this one, made where it is first named.
    fn at(&mut self, group: &[&'static str]) -> &mut Grouped {
        let Some((first, rest)) = group.split_first() else {
            return self;
        };
        let found = self
            .items
            .iter()
            .position(|item| matches!(item, Item::Group(name, _) if name == first));
        let at = found.unwrap_or_else(|| {
            self.items.push(Item::Group(first, Grouped::default()));
            self.items.len() - 1
        });
        match &mut self.items[at] {
            Item::Group(_, inner) => inner.at(rest),
            Item::Entry(_) => unreachable!("found a group there"),
        }
    }

    /// The rows and cards of the struct at `path`; `within` is the group they are in
    /// (`"Reuse/Temporal"`), empty at the struct's own body.
    fn scenes(self, cx: &BuildCx, path: &str, within: &str) -> Vec<Box<dyn Scene>> {
        self.items
            .into_iter()
            .map(|item| match item {
                Item::Entry(entry) => entry,
                Item::Group(name, inner) => {
                    let group = if within.is_empty() {
                        name.to_string()
                    } else {
                        format!("{within}/{name}")
                    };
                    let body = Box::new(column(inner.scenes(cx, path, &group)));
                    Box::new(attribute_group_card(cx, path, &group, name, body)) as Box<dyn Scene>
                }
            })
            .collect()
    }
}

/// On the card of a [`Group`](crate::attributes::Group) of a struct's fields, for an app that
/// decorates it (how many of its rows are overridden, say). Its header carries
/// [`InspectorGroupHeader`] and its body [`InspectorGroupBody`].
#[derive(Component, Clone, Default)]
pub struct InspectorGroup {
    /// Where the struct lives (`None` only on a default-constructed marker).
    pub root: Option<InspectorRoot>,
    /// The reflection path from the root to the struct whose fields these are.
    pub path: String,
    /// The group, outermost first, `/` between (`"Reuse/Temporal"`).
    pub group: String,
}

/// The header of an [`InspectorGroup`]'s card: the chevron and the title are its children.
#[derive(Component, Clone, Default)]
pub struct InspectorGroupHeader;

/// The body of an [`InspectorGroup`]'s card, a column: what an app adds under it is shown and
/// hidden with the group's own rows.
#[derive(Component, Clone, Default)]
pub struct InspectorGroupBody;

/// The key a group's collapsed state goes by in [`InspectorCollapsed`]: the struct's path, then
/// the group. No reflection path holds a `#`, so it is no field's key.
pub fn group_key(path: &str, group: &str) -> String {
    format!("{path}#{group}")
}

/// The card of a [`Group`](crate::attributes::Group): [`group_card`], marked for the app.
fn attribute_group_card(
    cx: &BuildCx,
    path: &str,
    within: &str,
    title: &str,
    body: Box<dyn Scene>,
) -> impl Scene {
    let key = group_key(path, within);
    let collapsed = cx.collapsed.is_collapsed(&cx.root, &key);
    let body_display = if collapsed {
        Display::None
    } else {
        Display::Flex
    };
    let marker = InspectorGroup {
        root: Some(cx.root.clone()),
        path: path.to_string(),
        group: within.to_string(),
    };
    let header: Vec<Box<dyn Scene>> = vec![
        collapse_toggle(&cx.root, &key, collapsed),
        Box::new(label(title.to_string())),
    ];
    let content: Vec<Box<dyn Scene>> = vec![body];
    bsn! {
        @group()
        CollapseGroup
        marker
        Node { width: percent(100) }
        Children [
            @group_header()
            InspectorGroupHeader
            Node { justify_content: JustifyContent::Start, column_gap: px(6) }
            on(on_header_click)
            Children [ {header} ]
            --
            @group_body()
            CollapseBody
            InspectorGroupBody
            Node { width: percent(100), display: {body_display} }
            Children [ {content} ]
        ]
    }
}

/// Recurse into a tuple struct, one entry per positional field.
fn build_tuple_struct(cx: &BuildCx, path: &str, tuple_struct: &dyn TupleStruct) -> Box<dyn Scene> {
    let rows: Vec<Box<dyn Scene>> = (0..tuple_struct.field_len())
        .filter_map(|i| {
            let child = tuple_struct.field(i)?;
            let child_path = format!("{path}.{i}");
            Some(field_entry(
                cx,
                &i.to_string(),
                &child_path,
                child,
                &FieldCtx::default(),
            ))
        })
        .collect();
    Box::new(column(rows))
}

/// On every field row, and on the header of the card of a compound field (a struct, an enum, a
/// list): the value it edits, for an app that decorates rows (a keyframe toggle, an override
/// marker).
#[derive(Component, Clone, Default)]
pub struct InspectorField {
    /// Where the value lives (`None` only on a default-constructed marker).
    pub root: Option<InspectorRoot>,
    /// The reflection path from the root to the field.
    pub path: String,
}

/// Render one field: a labeled row for scalars, or a titled `group` card for compound values.
pub(crate) fn field_entry(
    cx: &BuildCx,
    name: &str,
    path: &str,
    child: &dyn PartialReflect,
    field: &FieldCtx,
) -> Box<dyn Scene> {
    let compound = is_compound(cx, child, field);
    let widget = build_value(cx, path, child, field);
    if compound {
        Box::new(group_card(cx, path, name, widget))
    } else {
        let marker = InspectorField {
            root: Some(cx.root.clone()),
            path: path.to_string(),
        };
        Box::new(field_row(name, widget, marker))
    }
}

/// Whether `value` recurses into a nested structure (so it should get its own card).
fn is_compound(cx: &BuildCx, value: &dyn PartialReflect, field: &FieldCtx) -> bool {
    if field.read_only {
        return false;
    }
    if let Some(type_id) = value
        .get_represented_type_info()
        .map(bevy_reflect::TypeInfo::type_id)
        && cx
            .registry
            .get_type_data::<ReflectInspectorWidget>(type_id)
            .is_some()
    {
        return false;
    }
    matches!(
        value.reflect_ref(),
        ReflectRef::Struct(_)
            | ReflectRef::TupleStruct(_)
            | ReflectRef::Enum(_)
            | ReflectRef::List(_)
            | ReflectRef::Array(_)
    )
}

/// Parse a reflection path string, treating the empty string as the identity (root) path.
pub fn parse_path(path: &str) -> ParsedPath {
    if path.is_empty() {
        ParsedPath(Vec::new())
    } else {
        ParsedPath::parse(path).unwrap_or_else(|_| ParsedPath(Vec::new()))
    }
}

/// A labeled row that fills its width: a fixed-width name cell, then the editing widget.
pub fn field_row(name: &str, widget: Box<dyn Scene>, marker: InspectorField) -> impl Scene {
    let label_cell: Vec<Box<dyn Scene>> = vec![Box::new(label(name.to_string()))];
    let widget_cell: Vec<Box<dyn Scene>> = vec![widget];
    bsn! {
        Node {
            width: percent(100),
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            min_height: px(26),
        }
        marker
        Children [
            Node { width: px(96), flex_shrink: 0.0 }
            Children [ {label_cell} ]
            --
            {widget_cell}
        ]
    }
}

/// A full-width vertical stack of rows.
pub fn column(rows: Vec<Box<dyn Scene>>) -> impl Scene {
    bsn! {
        Node {
            width: percent(100),
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: px(2),
        }
        Children [ {rows} ]
    }
}

/// A titled feathers `group` card wrapping a body scene, filling its width.
///
/// The header carries a disclosure chevron that hides the body; `path` keys that state so it
/// survives panel rebuilds. See [`collapse`](crate::collapse).
pub(crate) fn group_card(
    cx: &BuildCx,
    path: &str,
    title: &str,
    body: Box<dyn Scene>,
) -> impl Scene {
    let collapsed = cx.collapsed.is_collapsed(&cx.root, path);
    let body_display = if collapsed {
        Display::None
    } else {
        Display::Flex
    };
    let header: Vec<Box<dyn Scene>> = vec![
        collapse_toggle(&cx.root, path, collapsed),
        Box::new(label(title.to_string())),
    ];
    let content: Vec<Box<dyn Scene>> = vec![body];
    // A card's header is its row: what an app puts beside a scalar's row it puts here.
    let marker = InspectorField {
        root: Some(cx.root.clone()),
        path: path.to_string(),
    };
    bsn! {
        @group()
        CollapseGroup
        Node { width: percent(100) }
        Children [
            @group_header()
            // The chevron and the title read as one unit, so override the header's
            // space-between default.
            Node { justify_content: JustifyContent::Start, column_gap: px(6) }
            marker
            on(on_header_click)
            Children [ {header} ]
            --
            @group_body()
            CollapseBody
            Node { width: percent(100), display: {body_display} }
            Children [ {content} ]
        ]
    }
}

/// A read-only fallback label showing the value (or its type when it can't be stringified).
fn read_only_leaf(value: &dyn PartialReflect) -> impl Scene {
    label_dim(value_to_string(value))
}

/// Best-effort stringification of a reflected value for read-only display.
pub fn value_to_string(value: &dyn PartialReflect) -> String {
    if let Some(reflect) = value.try_as_reflect() {
        if let Some(v) = reflect.downcast_ref::<f32>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<f64>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<i32>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<i64>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<u32>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<usize>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<bool>() {
            return v.to_string();
        }
        if let Some(v) = reflect.downcast_ref::<String>() {
            return v.clone();
        }
    }
    // Containers summarise; the TYPE PATH of a `HashMap<K, V>` is a wall of generics that
    // tells the reader nothing about the value in front of them.
    match value.reflect_ref() {
        ReflectRef::Map(map) => return format!("{{{} entries}}", map.len()),
        ReflectRef::Set(set) => return format!("{{{} items}}", set.len()),
        ReflectRef::List(list) => return format!("[{} items]", list.len()),
        ReflectRef::Array(array) => return format!("[{} items]", array.len()),
        _ => {}
    }
    // Anything else: whatever `Debug` offers, kept to one line.
    let text = format!("{value:?}");
    if text.is_empty() {
        return value
            .get_represented_type_info()
            .map(|info| info.type_path().to_string())
            .unwrap_or_else(|| "<opaque>".to_string());
    }
    match text.char_indices().nth(96) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}
