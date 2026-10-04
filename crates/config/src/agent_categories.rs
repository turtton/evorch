//! Agent category metadata shared by config validation, prompts and delegation tools.

/// A worker category that may be selected through the public delegation tool.
///
/// This compatibility view is limited to workers; use [`public_categories`] for
/// tool enums and selection guidance spanning all public roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicWorkerCategory {
    pub id: CategoryId,
    pub guidance: &'static str,
}

/// A public delegation category with its owning role and selection criteria.
///
/// Use [`public_categories`] for model-facing schemas and guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicCategory {
    pub id: CategoryId,
    pub role: &'static str,
    pub guidance: &'static str,
}

/// A category whose model and generation settings can be edited in the GUI.
///
/// Settings visibility is independent of public delegation: shell execution
/// audits have a configurable reviewer binding without being delegatable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsCategory {
    pub id: CategoryId,
    pub role: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum Delegation {
    Public { guidance: &'static str },
    Internal,
}

pub(crate) struct CategoryDefinition {
    pub id: CategoryId,
    pub role: &'static str,
    delegation: Delegation,
    settings_visible: bool,
    pub overlay_preset: &'static str,
    pub overlay_body: &'static str,
}

// One declaration generates both typed IDs and their ordered metadata registry.
macro_rules! define_categories {
    ($($variant:ident { name: $name:literal, role: $role:literal,
        settings_visible: $visible:literal, delegation: $delegation:expr,
        overlay_preset: $preset:literal, overlay_body: $body:expr,
    })*) => {
        /// A registered category shared by config, the GUI, and runtime code.
        /// Convert to its canonical string only at external storage/tool boundaries.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum CategoryId { $($variant),* }

        impl CategoryId {
            /// Registry order is part of the model-facing schema contract.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),* }
            }

            /// Parse an exact canonical name, including internal categories.
            pub fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)* _ => None }
            }

            const fn definition(self) -> &'static CategoryDefinition {
                &CATEGORIES[self as usize]
            }

            pub const fn role(self) -> &'static str { self.definition().role }
            pub const fn settings_visible(self) -> bool { self.definition().settings_visible }
            pub const fn public_guidance(self) -> Option<&'static str> {
                match self.definition().delegation {
                    Delegation::Public { guidance } => Some(guidance),
                    Delegation::Internal => None,
                }
            }
            pub const fn overlay_preset(self) -> &'static str {
                self.definition().overlay_preset
            }
        }

        impl std::fmt::Display for CategoryId {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        pub(crate) const CATEGORIES: &[CategoryDefinition] = &[$(
            CategoryDefinition {
                id: CategoryId::$variant, role: $role,
                settings_visible: $visible, delegation: $delegation,
                overlay_preset: $preset, overlay_body: $body,
            }
        ),*];
    };
}

// Order is part of the model-facing tool schema and must remain stable.
define_categories! {
    Quick {
        name: "quick",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "bounded, well-specified mechanical work such as a typo, localized fix, or routine commit of reviewed changes with an exact staging scope; give explicit checks and forbidden actions.",
        },
        overlay_preset: "category-quick",
        overlay_body: include_str!("../assets/presets/category-quick.md"),
    }
    Deep {
        name: "deep",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "multi-step implementation requiring codebase investigation, dependent edits, or broad verification.",
        },
        overlay_preset: "category-deep",
        overlay_body: include_str!("../assets/presets/category-deep.md"),
    }
    HighReasoning {
        name: "high-reasoning",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "subtle invariants, hard debugging, or competing designs where reasoning is the bottleneck, even with few files; use deep when breadth is the main challenge.",
        },
        overlay_preset: "category-high-reasoning",
        overlay_body: include_str!("../assets/presets/category-high-reasoning.md"),
    }
    Visual {
        name: "visual",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "UI layout, styling, design, or screenshot-driven visual work; use multimodal_looker for image interpretation alone.",
        },
        overlay_preset: "category-visual",
        overlay_body: include_str!("../assets/presets/category-visual.md"),
    }
    Writing {
        name: "writing",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "documentation, prose, or copy where audience and wording dominate.",
        },
        overlay_preset: "category-writing",
        overlay_body: include_str!("../assets/presets/category-writing.md"),
    }
    Research {
        name: "research",
        role: "worker",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "multi-source evidence synthesis with a worker deliverable; use explorer for read-only local code investigation and web_researcher for external source collection alone.",
        },
        overlay_preset: "category-research",
        overlay_body: include_str!("../assets/presets/category-research.md"),
    }
    PlanReview {
        name: "plan-review",
        role: "reviewer",
        settings_visible: true,
        delegation: Delegation::Public {
            guidance: "review a planner-produced plan before execution: requirement coverage, feasibility, task decomposition, dependency ordering, risks, and missing acceptance criteria.",
        },
        overlay_preset: "category-plan-review",
        overlay_body: include_str!("../assets/presets/category-plan-review.md"),
    }
    ToolExecution {
        name: "tool-execution",
        role: "reviewer",
        settings_visible: true,
        delegation: Delegation::Internal,
        overlay_preset: "category-tool-execution",
        overlay_body: include_str!("../assets/presets/category-tool-execution.md"),
    }
    Conversation {
        name: "conversation",
        role: "worker",
        settings_visible: false,
        delegation: Delegation::Internal,
        overlay_preset: "category-conversation",
        overlay_body: include_str!("../assets/presets/category-conversation.md"),
    }
    Lesson {
        name: "lesson",
        role: "worker",
        settings_visible: false,
        delegation: Delegation::Internal,
        overlay_preset: "category-lesson",
        overlay_body: include_str!("../assets/presets/category-lesson.md"),
    }
    LessonReview {
        name: "lesson_review",
        role: "reviewer",
        settings_visible: false,
        delegation: Delegation::Internal,
        overlay_preset: "category-lesson-review",
        overlay_body: include_str!("../assets/presets/category-lesson-review.md"),
    }
}

/// Enumerate all public categories with their owning roles in stable order.
pub fn public_categories() -> impl Iterator<Item = PublicCategory> {
    CATEGORIES.iter().filter_map(|category| {
        if let Delegation::Public { guidance } = category.delegation {
            Some(PublicCategory {
                id: category.id,
                role: category.role,
                guidance,
            })
        } else {
            None
        }
    })
}

/// Return the owning role only for publicly delegatable categories.
pub fn public_category_role(name: &str) -> Option<&'static str> {
    CategoryId::parse(name)
        .filter(|category| category.public_guidance().is_some())
        .map(CategoryId::role)
}

/// Enumerate categories exposed in role settings, including internal shell audits.
pub fn settings_categories() -> impl Iterator<Item = SettingsCategory> {
    CATEGORIES
        .iter()
        .filter(|category| category.settings_visible)
        .map(|category| SettingsCategory {
            id: category.id,
            role: category.role,
        })
}

/// Enumerate public worker categories and their selection criteria in stable order.
///
/// This compatibility view excludes reviewer and internal categories.
pub fn public_worker_categories() -> impl Iterator<Item = PublicWorkerCategory> {
    public_categories()
        .filter(|category| category.role == "worker")
        .map(|category| PublicWorkerCategory {
            id: category.id,
            guidance: category.guidance,
        })
}

/// Whether a category may be selected for a worker through public delegation.
pub fn is_public_worker_category(name: &str) -> bool {
    public_category_role(name) == Some("worker")
}

/// Overlay preset name for any registered category, including internal ones.
pub fn overlay_preset_for(name: &str) -> Option<&'static str> {
    CategoryId::parse(name).map(CategoryId::overlay_preset)
}

pub(crate) fn categories_for_role(
    role: &str,
) -> impl Iterator<Item = &'static CategoryDefinition> + '_ {
    CATEGORIES
        .iter()
        .filter(move |category| category.role == role)
}

pub(crate) fn category_for_role(role: &str, name: &str) -> Option<&'static CategoryDefinition> {
    CategoryId::parse(name)
        .filter(|category| category.role() == role)
        .map(CategoryId::definition)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_worker_categories_preserve_order_and_have_selection_guidance() {
        let categories: Vec<_> = public_worker_categories().collect();
        assert_eq!(
            categories
                .iter()
                .map(|category| category.id.as_str())
                .collect::<Vec<_>>(),
            [
                "quick",
                "deep",
                "high-reasoning",
                "visual",
                "writing",
                "research"
            ]
        );
        for category in categories {
            assert!(
                !category.guidance.is_empty(),
                "{} needs guidance",
                category.id.as_str()
            );
            assert!(category_for_role("worker", category.id.as_str()).is_some());
        }
    }

    #[test]
    fn internal_categories_are_resolvable_but_never_publicly_delegatable() {
        for (name, role) in [
            ("conversation", "worker"),
            ("lesson", "worker"),
            ("lesson_review", "reviewer"),
            ("tool-execution", "reviewer"),
        ] {
            let definition = category_for_role(role, name).expect("internal category exists");
            assert!(matches!(definition.delegation, Delegation::Internal));
            assert!(!is_public_worker_category(name));
            assert_eq!(public_category_role(name), None);
        }
        for name in ["", "unknown", "Quick", "lesson-review"] {
            assert!(!is_public_worker_category(name));
            assert_eq!(public_category_role(name), None);
        }
        assert!(category_for_role("worker", "lesson_review").is_none());
        assert!(category_for_role("reviewer", "lesson").is_none());
    }

    #[test]
    fn public_reviewer_categories_have_roles_and_do_not_enter_worker_projection() {
        let categories: Vec<_> = public_categories().collect();
        assert_eq!(categories.len(), 7);
        assert_eq!(
            categories[6..]
                .iter()
                .map(|category| category.id.as_str())
                .collect::<Vec<_>>(),
            ["plan-review"]
        );
        assert_eq!(public_category_role("plan-review"), Some("reviewer"));
        for category in categories {
            assert_eq!(
                public_category_role(category.id.as_str()),
                Some(category.role)
            );
            assert!(!category.guidance.is_empty());
            assert!(category_for_role(category.role, category.id.as_str()).is_some());
            if category.role == "reviewer" {
                assert!(!is_public_worker_category(category.id.as_str()));
            }
        }
    }

    #[test]
    fn settings_categories_include_shell_audits_without_exposing_other_internal_bindings() {
        let categories: Vec<_> = settings_categories().collect();
        assert_eq!(
            categories
                .iter()
                .filter(|category| category.role == "reviewer")
                .map(|category| category.id.as_str())
                .collect::<Vec<_>>(),
            ["plan-review", "tool-execution"]
        );
        for category in categories {
            assert!(category_for_role(category.role, category.id.as_str()).is_some());
            assert!(!matches!(
                category.id.as_str(),
                "conversation" | "lesson" | "lesson_review"
            ));
        }
    }

    #[test]
    fn canonical_ids_round_trip_and_overlay_presets_are_unique() {
        let mut names = std::collections::BTreeSet::new();
        let mut presets = std::collections::BTreeSet::new();
        for &id in CategoryId::ALL {
            assert_eq!(CategoryId::parse(id.as_str()), Some(id));
            assert_eq!(id.to_string(), id.as_str());
            let category = id.definition();
            assert!(
                names.insert(category.id.as_str()),
                "duplicate category {}",
                category.id.as_str()
            );
            assert!(
                presets.insert(category.overlay_preset),
                "duplicate category preset"
            );
            assert!(!category.overlay_body.is_empty());
            if matches!(category.delegation, Delegation::Public { .. }) {
                assert!(matches!(category.role, "worker" | "reviewer"));
            }
        }
        assert_eq!(CategoryId::LessonReview.as_str(), "lesson_review");
        assert_eq!(CategoryId::parse("lesson-review"), None);
        assert_eq!(CategoryId::parse("Quick"), None);
        assert_eq!(CategoryId::parse("unknown"), None);
    }
}
