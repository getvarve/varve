//! Rewrite rules and row level security policies.

use serde::{Deserialize, Serialize};
use varve_driver::ServerInfo;

use super::common::{
    DropBehavior, FiringState, PlanBuilder, QualifiedName, RoleSpec, comment_on, expr, ident,
    invalid, paren_expr, statement,
};
use crate::model::{CatalogResult, EditPlan};

/// The command a rule rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum RuleEvent {
    /// `SELECT` (only `_RETURN` rules of views; `DO INSTEAD` one select).
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

/// `CREATE [OR REPLACE] RULE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreateRule {
    /// Name.
    pub name: String,
    /// The table or view.
    pub table: QualifiedName,
    /// `OR REPLACE`.
    #[serde(default)]
    pub or_replace: bool,
    /// Event.
    pub event: RuleEvent,
    /// `WHERE` condition.
    #[serde(default)]
    pub condition: Option<String>,
    /// `DO INSTEAD` when true, `DO ALSO` when false.
    #[serde(default)]
    pub instead: bool,
    /// Commands, each without `;`; empty means `NOTHING`.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RuleChange {
    /// Create or replace a rule.
    Create(CreateRule),
    /// Rename.
    Rename {
        /// Rule.
        name: String,
        /// Table.
        table: QualifiedName,
        /// New name.
        new_name: String,
    },
    /// Enable or disable.
    SetState {
        /// Rule.
        name: String,
        /// Table.
        table: QualifiedName,
        /// State.
        state: FiringState,
    },
    /// Drop.
    Drop {
        /// Rule.
        name: String,
        /// Table.
        table: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
        /// Dependent objects.
        #[serde(default)]
        behavior: DropBehavior,
    },
}

pub(crate) fn plan_rule(_server: &ServerInfo, change: &RuleChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        RuleChange::Create(r) => {
            let table = r.table.sql()?;
            let mut s = format!(
                "CREATE {}RULE {} AS ON {} TO {table}",
                if r.or_replace { "OR REPLACE " } else { "" },
                ident(&r.name)?,
                match r.event {
                    RuleEvent::Select => "SELECT",
                    RuleEvent::Insert => "INSERT",
                    RuleEvent::Update => "UPDATE",
                    RuleEvent::Delete => "DELETE",
                }
            );
            if let Some(c) = &r.condition {
                s.push_str(&format!("\nWHERE {}", paren_expr(c, "a rule condition")?));
            }
            s.push_str(if r.instead {
                "\nDO INSTEAD "
            } else {
                "\nDO ALSO "
            });
            let actions = r
                .actions
                .iter()
                .map(|a| statement(a, "a rule command"))
                .collect::<CatalogResult<Vec<_>>>()?;
            match actions.len() {
                0 => s.push_str("NOTHING"),
                1 => s.push_str(&actions[0]),
                _ => s.push_str(&format!("({})", actions.join("; "))),
            }
            plan.push(s);
            if let Some(c) = &r.comment {
                plan.push(comment_on(
                    &format!("RULE {} ON {table}", ident(&r.name)?),
                    Some(c),
                )?);
            }
        }
        RuleChange::Rename {
            name,
            table,
            new_name,
        } => plan.push(format!(
            "ALTER RULE {} ON {} RENAME TO {}",
            ident(name)?,
            table.sql()?,
            ident(new_name)?
        )),
        RuleChange::SetState { name, table, state } => {
            plan.warn(format!(
                "enabling or disabling a rule {}",
                super::common::ACCESS_EXCLUSIVE
            ));
            plan.push(format!(
                "ALTER TABLE {} {} RULE {}",
                table.sql()?,
                state.sql(),
                ident(name)?
            ));
        }
        RuleChange::Drop {
            name,
            table,
            if_exists,
            behavior,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP RULE {}{} ON {} {}",
                if *if_exists { "IF EXISTS " } else { "" },
                ident(name)?,
                table.sql()?,
                behavior.sql()
            ));
        }
    }
    plan.finish()
}

/// The commands a policy applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub enum PolicyCommand {
    /// `ALL` (the default).
    #[default]
    All,
    /// `SELECT`.
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

/// `CREATE POLICY`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct CreatePolicy {
    /// Name.
    pub name: String,
    /// The table.
    pub table: QualifiedName,
    /// `AS RESTRICTIVE` when true (combined with AND); permissive policies
    /// are combined with OR.
    #[serde(default)]
    pub restrictive: bool,
    /// Commands.
    #[serde(default)]
    pub command: PolicyCommand,
    /// Roles; empty means `PUBLIC`.
    #[serde(default)]
    pub roles: Vec<RoleSpec>,
    /// `USING`: which existing rows are visible (not for `INSERT`).
    #[serde(default)]
    pub using: Option<String>,
    /// `WITH CHECK`: which new rows are allowed (not for `SELECT`, `DELETE`).
    #[serde(default)]
    pub with_check: Option<String>,
    /// Comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// Changes to policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PolicyChange {
    /// Create a policy.
    Create(CreatePolicy),
    /// Alter a policy. The command and permissiveness cannot change; drop
    /// and recreate for that.
    Alter {
        /// Policy.
        name: String,
        /// Table.
        table: QualifiedName,
        /// New roles.
        #[serde(default)]
        roles: Option<Vec<RoleSpec>>,
        /// New `USING`.
        #[serde(default)]
        using: Option<String>,
        /// New `WITH CHECK`.
        #[serde(default)]
        with_check: Option<String>,
        /// New name, applied after the other changes.
        #[serde(default)]
        new_name: Option<String>,
    },
    /// Drop a policy.
    Drop {
        /// Policy.
        name: String,
        /// Table.
        table: QualifiedName,
        /// `IF EXISTS`.
        #[serde(default)]
        if_exists: bool,
    },
    /// Replace a policy whose command or permissiveness changes, or whose
    /// `USING` / `WITH CHECK` is removed (what `ALTER POLICY` cannot do):
    /// `DROP POLICY` and `CREATE POLICY` in one transaction, so a failing
    /// create leaves the old policy in place.
    Replace {
        /// The policy as it is called now.
        name: String,
        /// The new definition (its `table` must be the same table).
        policy: CreatePolicy,
    },
}

fn roles_sql(server: &ServerInfo, roles: &[RoleSpec]) -> CatalogResult<String> {
    if roles.is_empty() {
        return Err(invalid("a policy needs at least one role (or PUBLIC)"));
    }
    Ok(roles
        .iter()
        .map(|r| r.sql(server))
        .collect::<CatalogResult<Vec<_>>>()?
        .join(", "))
}

fn create_policy(
    server: &ServerInfo,
    p: &CreatePolicy,
    plan: &mut PlanBuilder,
) -> CatalogResult<()> {
    if p.command == PolicyCommand::Insert && p.using.is_some() {
        return Err(invalid("an INSERT policy has only WITH CHECK"));
    }
    if matches!(p.command, PolicyCommand::Select | PolicyCommand::Delete) && p.with_check.is_some()
    {
        return Err(invalid("SELECT and DELETE policies have only USING"));
    }
    let table = p.table.sql()?;
    let mut s = format!("CREATE POLICY {} ON {table}", ident(&p.name)?);
    if p.restrictive {
        s.push_str(" AS RESTRICTIVE");
    }
    s.push_str(match p.command {
        PolicyCommand::All => " FOR ALL",
        PolicyCommand::Select => " FOR SELECT",
        PolicyCommand::Insert => " FOR INSERT",
        PolicyCommand::Update => " FOR UPDATE",
        PolicyCommand::Delete => " FOR DELETE",
    });
    if !p.roles.is_empty() {
        s.push_str(&format!(" TO {}", roles_sql(server, &p.roles)?));
    }
    if let Some(u) = &p.using {
        s.push_str(&format!("\nUSING ({})", expr(u, "a USING expression")?));
    }
    if let Some(c) = &p.with_check {
        s.push_str(&format!(
            "\nWITH CHECK ({})",
            expr(c, "a WITH CHECK expression")?
        ));
    }
    plan.warn("policies only take effect when row level security is enabled on the table");
    plan.push(s);
    if let Some(c) = &p.comment {
        plan.push(comment_on(
            &format!("POLICY {} ON {table}", ident(&p.name)?),
            Some(c),
        )?);
    }
    Ok(())
}

pub(crate) fn plan_policy(server: &ServerInfo, change: &PolicyChange) -> CatalogResult<EditPlan> {
    let mut plan = PlanBuilder::new();
    match change {
        PolicyChange::Create(p) => create_policy(server, p, &mut plan)?,
        PolicyChange::Replace { name, policy } => {
            // Both statements run in the plan's one transaction: when the
            // create fails, the drop is rolled back with it.
            plan.destructive();
            plan.warn("the policy is dropped and created again in one transaction; if the create fails, nothing changes");
            plan.push(format!(
                "DROP POLICY {} ON {}",
                ident(name)?,
                policy.table.sql()?
            ));
            create_policy(server, policy, &mut plan)?;
        }
        PolicyChange::Alter {
            name,
            table,
            roles,
            using,
            with_check,
            new_name,
        } => {
            let head = format!("ALTER POLICY {} ON {}", ident(name)?, table.sql()?);
            let mut s = head.clone();
            if let Some(r) = roles {
                s.push_str(&format!(" TO {}", roles_sql(server, r)?));
            }
            if let Some(u) = using {
                s.push_str(&format!(" USING ({})", expr(u, "a USING expression")?));
            }
            if let Some(c) = with_check {
                s.push_str(&format!(
                    " WITH CHECK ({})",
                    expr(c, "a WITH CHECK expression")?
                ));
            }
            if s != head {
                plan.push(s);
            }
            if let Some(n) = new_name {
                plan.push(format!("{head} RENAME TO {}", ident(n)?));
            }
        }
        PolicyChange::Drop {
            name,
            table,
            if_exists,
        } => {
            plan.destructive();
            plan.push(format!(
                "DROP POLICY {}{} ON {}",
                if *if_exists { "IF EXISTS " } else { "" },
                ident(name)?,
                table.sql()?
            ));
        }
    }
    plan.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::edit::common::tests::pg;

    fn t() -> QualifiedName {
        QualifiedName::new("s", "t")
    }

    #[test]
    fn rules() -> CatalogResult<()> {
        let base = CreateRule {
            name: "r".into(),
            table: t(),
            or_replace: true,
            event: RuleEvent::Insert,
            condition: Some("NEW.a < 0".into()),
            instead: true,
            actions: vec![],
            comment: Some("c".into()),
        };
        assert_eq!(
            plan_rule(&pg(13), &RuleChange::Create(base.clone()))?.statements,
            vec![
                "CREATE OR REPLACE RULE \"r\" AS ON INSERT TO \"s\".\"t\"\nWHERE (NEW.a < 0)\nDO INSTEAD NOTHING",
                r#"COMMENT ON RULE "r" ON "s"."t" IS 'c'"#,
            ]
        );
        let two = CreateRule {
            or_replace: false,
            event: RuleEvent::Delete,
            condition: None,
            instead: false,
            actions: vec!["INSERT INTO s.log VALUES (OLD.a)".into(), "NOTIFY t".into()],
            comment: None,
            ..base
        };
        assert_eq!(
            plan_rule(&pg(13), &RuleChange::Create(two))?.statements,
            vec![
                "CREATE RULE \"r\" AS ON DELETE TO \"s\".\"t\"\nDO ALSO (INSERT INTO s.log VALUES (OLD.a); NOTIFY t)"
            ]
        );
        let cases = [
            (
                RuleChange::Rename {
                    name: "r".into(),
                    table: t(),
                    new_name: "q".into(),
                },
                r#"ALTER RULE "r" ON "s"."t" RENAME TO "q""#,
            ),
            (
                RuleChange::SetState {
                    name: "r".into(),
                    table: t(),
                    state: FiringState::Disabled,
                },
                r#"ALTER TABLE "s"."t" DISABLE RULE "r""#,
            ),
            (
                RuleChange::Drop {
                    name: "r".into(),
                    table: t(),
                    if_exists: true,
                    behavior: DropBehavior::Restrict,
                },
                r#"DROP RULE IF EXISTS "r" ON "s"."t" RESTRICT"#,
            ),
        ];
        for (change, sql) in cases {
            let p = plan_rule(&pg(13), &change)?;
            assert_eq!(p.statements, vec![sql]);
            if matches!(change, RuleChange::SetState { .. }) {
                assert!(p.warnings.iter().any(|w| w.contains("ACCESS EXCLUSIVE")));
            }
        }
        Ok(())
    }

    #[test]
    fn policies() -> CatalogResult<()> {
        let p = CreatePolicy {
            name: "own".into(),
            table: t(),
            restrictive: true,
            command: PolicyCommand::Update,
            roles: vec![RoleSpec::name("app"), RoleSpec::CurrentUser],
            using: Some("owner = current_user".into()),
            with_check: Some("owner = current_user".into()),
            comment: Some("mine".into()),
        };
        let plan = plan_policy(&pg(13), &PolicyChange::Create(p.clone()))?;
        assert_eq!(
            plan.statements,
            vec![
                "CREATE POLICY \"own\" ON \"s\".\"t\" AS RESTRICTIVE FOR UPDATE TO \"app\", CURRENT_USER\nUSING (owner = current_user)\nWITH CHECK (owner = current_user)",
                r#"COMMENT ON POLICY "own" ON "s"."t" IS 'mine'"#,
            ]
        );
        assert!(
            plan_policy(
                &pg(13),
                &PolicyChange::Create(CreatePolicy {
                    command: PolicyCommand::Insert,
                    ..p.clone()
                })
            )
            .is_err()
        );
        assert!(
            plan_policy(
                &pg(13),
                &PolicyChange::Create(CreatePolicy {
                    command: PolicyCommand::Select,
                    using: None,
                    ..p
                })
            )
            .is_err()
        );
        let alter = plan_policy(
            &pg(13),
            &PolicyChange::Alter {
                name: "own".into(),
                table: t(),
                roles: Some(vec![RoleSpec::Public]),
                using: Some("true".into()),
                with_check: None,
                new_name: Some("all".into()),
            },
        )?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER POLICY "own" ON "s"."t" TO PUBLIC USING (true)"#,
                r#"ALTER POLICY "own" ON "s"."t" RENAME TO "all""#,
            ]
        );
        let drop = plan_policy(
            &pg(13),
            &PolicyChange::Drop {
                name: "own".into(),
                table: t(),
                if_exists: true,
            },
        )?;
        assert_eq!(
            drop.statements,
            vec![r#"DROP POLICY IF EXISTS "own" ON "s"."t""#]
        );
        assert!(drop.destructive);
        let replace = plan_policy(
            &pg(13),
            &PolicyChange::Replace {
                name: "own".into(),
                policy: CreatePolicy {
                    name: "own2".into(),
                    restrictive: false,
                    command: PolicyCommand::All,
                    roles: vec![],
                    using: Some("true".into()),
                    with_check: None,
                    comment: None,
                    table: t(),
                },
            },
        )?;
        assert_eq!(
            replace.statements,
            vec![
                r#"DROP POLICY "own" ON "s"."t""#,
                "CREATE POLICY \"own2\" ON \"s\".\"t\" FOR ALL\nUSING (true)",
            ]
        );
        assert!(replace.transactional && replace.destructive);
        Ok(())
    }
}
