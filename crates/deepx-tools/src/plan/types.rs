//! PLAN.md item type used by parse_plan_items().

#[derive(serde::Serialize, Clone)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub deps: String,
    pub effort: String,
    pub comment: String,
}