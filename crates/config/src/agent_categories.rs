//! Agent category metadata shared by config validation, prompts and delegation tools.

/// A worker category that may be selected through the public delegation tool.
///
/// Construct tool enums and selection guidance from [`public_worker_categories`]
/// so internal categories cannot enter the model-facing contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicWorkerCategory {
    pub name: &'static str,
    pub guidance: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum Delegation {
    Public { guidance: &'static str },
    Internal,
}

pub(crate) struct CategoryDefinition {
    pub name: &'static str,
    pub role: &'static str,
    delegation: Delegation,
    pub overlay_preset: &'static str,
    pub overlay_body: &'static str,
}

// Order is part of the model-facing tool schema and must remain stable.
pub(crate) const CATEGORIES: &[CategoryDefinition] = &[
    CategoryDefinition {
        name: "quick",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "bounded, well-specified mechanical work such as a typo, localized fix, or routine commit of reviewed changes with an exact staging scope; give explicit checks and forbidden actions.",
        },
        overlay_preset: "category-quick",
        overlay_body: include_str!("../assets/presets/category-quick.md"),
    },
    CategoryDefinition {
        name: "deep",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "multi-step implementation requiring codebase investigation, dependent edits, or broad verification.",
        },
        overlay_preset: "category-deep",
        overlay_body: include_str!("../assets/presets/category-deep.md"),
    },
    CategoryDefinition {
        name: "high-reasoning",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "subtle invariants, hard debugging, or competing designs where reasoning is the bottleneck, even with few files; use deep when breadth is the main challenge.",
        },
        overlay_preset: "category-high-reasoning",
        overlay_body: include_str!("../assets/presets/category-high-reasoning.md"),
    },
    CategoryDefinition {
        name: "visual",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "UI layout, styling, design, or screenshot-driven visual work; use multimodal_looker for image interpretation alone.",
        },
        overlay_preset: "category-visual",
        overlay_body: include_str!("../assets/presets/category-visual.md"),
    },
    CategoryDefinition {
        name: "writing",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "documentation, prose, or copy where audience and wording dominate.",
        },
        overlay_preset: "category-writing",
        overlay_body: include_str!("../assets/presets/category-writing.md"),
    },
    CategoryDefinition {
        name: "research",
        role: "worker",
        delegation: Delegation::Public {
            guidance: "multi-source evidence synthesis with a worker deliverable; use explorer for read-only local code investigation and web_researcher for external source collection alone.",
        },
        overlay_preset: "category-research",
        overlay_body: include_str!("../assets/presets/category-research.md"),
    },
    CategoryDefinition {
        name: "lesson",
        role: "worker",
        delegation: Delegation::Internal,
        overlay_preset: "category-lesson",
        overlay_body: include_str!("../assets/presets/category-lesson.md"),
    },
    CategoryDefinition {
        name: "lesson_review",
        role: "reviewer",
        delegation: Delegation::Internal,
        overlay_preset: "category-lesson-review",
        overlay_body: include_str!("../assets/presets/category-lesson-review.md"),
    },
];

/// Enumerate public worker categories and their selection criteria in stable order.
///
/// Internal categories have no public guidance and are excluded even when their
/// owning role is worker. Use this same projection for both tool schema and text.
pub fn public_worker_categories() -> impl Iterator<Item = PublicWorkerCategory> {
    CATEGORIES.iter().filter_map(|category| {
        if let Delegation::Public { guidance } = category.delegation
            && category.role == "worker"
        {
            Some(PublicWorkerCategory {
                name: category.name,
                guidance,
            })
        } else {
            None
        }
    })
}

/// Whether a category may be selected for a worker through public delegation.
pub fn is_public_worker_category(name: &str) -> bool {
    public_worker_categories().any(|category| category.name == name)
}

pub(crate) fn categories_for_role(
    role: &str,
) -> impl Iterator<Item = &'static CategoryDefinition> + '_ {
    CATEGORIES
        .iter()
        .filter(move |category| category.role == role)
}

pub(crate) fn category_for_role(role: &str, name: &str) -> Option<&'static CategoryDefinition> {
    categories_for_role(role).find(|category| category.name == name)
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
                .map(|category| category.name)
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
                category.name
            );
            assert!(is_public_worker_category(category.name));
            assert!(category_for_role("worker", category.name).is_some());
        }
    }

    #[test]
    fn internal_categories_are_resolvable_but_never_publicly_delegatable() {
        for (name, role) in [("lesson", "worker"), ("lesson_review", "reviewer")] {
            let definition = category_for_role(role, name).expect("internal category exists");
            assert!(matches!(definition.delegation, Delegation::Internal));
            assert!(!is_public_worker_category(name));
            assert!(public_worker_categories().all(|category| category.name != name));
        }
        for name in ["", "unknown", "Quick", "lesson-review"] {
            assert!(!is_public_worker_category(name));
        }
        assert!(category_for_role("worker", "lesson_review").is_none());
        assert!(category_for_role("reviewer", "lesson").is_none());
    }

    #[test]
    fn registry_names_and_overlay_presets_are_unique() {
        let mut names = std::collections::BTreeSet::new();
        let mut presets = std::collections::BTreeSet::new();
        for category in CATEGORIES {
            assert!(
                names.insert(category.name),
                "duplicate category {}",
                category.name
            );
            assert!(
                presets.insert(category.overlay_preset),
                "duplicate category preset"
            );
            assert!(!category.overlay_body.is_empty());
            if matches!(category.delegation, Delegation::Public { .. }) {
                assert_eq!(category.role, "worker");
            }
        }
    }
}
