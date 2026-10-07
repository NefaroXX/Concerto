//! Shared value types of the entities subsystem (NORM S6).
//!
//! One home for every persisted row shape — code entity, entity relation and
//! extracted fact — plus the SQL that creates their tables, so the writers and
//! readers of a shape sit side by side and a field rename is a single edit.

use concerto_core::memory::ProjectId;

/// A single code entity extracted from source code.
#[derive(Debug, Clone)]
pub struct CodeEntity {
    pub id: String,
    pub project_id: ProjectId,
    pub name: String,
    pub kind: EntityKind,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
}

/// Kind of code entity.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum EntityKind {
    Function,
    Struct,
    Trait,
    Impl,
    Enum,
    Interface,
    Class,
    Method,
    Module,
}

/// A relation between two code entities.
#[derive(Debug, Clone)]
pub struct EntityRelation {
    pub id: String,
    pub project_id: ProjectId,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation: RelationKind,
}

/// Kind of relation between entities.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum RelationKind {
    Imports,
    Extends,
    Implements,
    Calls,
    Contains,
}

/// An extracted architectural fact.
#[derive(Debug, Clone)]
pub struct FactEntry {
    pub id: String,
    pub project_id: ProjectId,
    pub content: String,
    pub category: FactCategory,
    pub source_file: Option<String>,
    pub confidence: Option<f32>,
    pub expires_at: Option<i64>,
}

/// Category of extracted fact.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum FactCategory {
    Architecture,
    Constraint,
    Pattern,
    Decision,
}

/// Migration SQL for the entities tables.
pub const MIGRATION_010_ENTITIES: &str = r#"
CREATE TABLE IF NOT EXISTS code_entities (
    id           TEXT    PRIMARY KEY,
    project_id   TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    kind         TEXT    NOT NULL
                 CHECK  (kind IN ('function','struct','trait','impl','enum',
                                  'interface','class','method','module')),
    file_path    TEXT    NOT NULL,
    line_start   INTEGER NOT NULL,
    line_end     INTEGER NOT NULL,
    signature    TEXT,
    doc_comment  TEXT,
    last_seen    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_relations (
    id               TEXT    PRIMARY KEY,
    project_id       TEXT    NOT NULL,
    source_entity_id TEXT    NOT NULL,
    target_entity_id TEXT    NOT NULL,
    relation         TEXT    NOT NULL
                     CHECK  (relation IN ('imports','extends','implements',
                                          'calls','contains')),
    last_seen        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_entries (
    id          TEXT    PRIMARY KEY,
    project_id  TEXT    NOT NULL,
    content     TEXT    NOT NULL,
    category    TEXT    NOT NULL
                CHECK  (category IN ('architecture','constraint',
                                     'pattern','decision')),
    source_file TEXT,
    confidence  REAL    CHECK (confidence IS NULL OR
                               (confidence >= 0.0 AND confidence <= 1.0)),
    expires_at  INTEGER,
    created_at  INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `EntityKind` variant keeps its stable `Debug` spelling (the
    /// value the migration `CHECK` constraints and rows are written with).
    #[test]
    fn entity_kind_debug_output() {
        assert_eq!(format!("{:?}", EntityKind::Function), "Function");
        assert_eq!(format!("{:?}", EntityKind::Struct), "Struct");
        assert_eq!(format!("{:?}", EntityKind::Enum), "Enum");
        assert_eq!(format!("{:?}", EntityKind::Trait), "Trait");
        assert_eq!(format!("{:?}", EntityKind::Impl), "Impl");
        assert_eq!(format!("{:?}", EntityKind::Class), "Class");
        assert_eq!(format!("{:?}", EntityKind::Interface), "Interface");
        assert_eq!(format!("{:?}", EntityKind::Method), "Method");
        assert_eq!(format!("{:?}", EntityKind::Module), "Module");
    }
}
