use super::*;
#[derive(Clone, Debug, Serialize)]
pub struct Preset {
    pub id: String,
    pub title: String,
    pub definition: Definition,
}
/// Templates are ordinary definitions. Applying one adds no hidden behaviour or
/// policy; callers can customise the result before the usual create command.
pub fn presets(start_date: &str, timezone: &str) -> Result<Vec<Preset>> {
    let base = Definition {
        schedule: Schedule {
            start_date: Some(start_date.into()),
            time: None,
            timezone: timezone.into(),
            repeat: Some(Cadence {
                frequency: Frequency::Daily,
                interval: 1,
            }),
        },
        goal: Goal::Checkbox,
        carry: Carry::CloseIncomplete,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    };
    base.validate()?;
    let mut chore = base.clone();
    chore.carry = Carry::RetainOne;
    chore.participation = Participation::Anyone;
    let mut quota = base.clone();
    quota.schedule.repeat = Some(Cadence {
        frequency: Frequency::Weekly,
        interval: 1,
    });
    quota.close_days_after = 7;
    quota.goal = Goal::Numeric {
        minimum: Some(Decimal("3".into())),
        maximum: None,
        unit: "times".into(),
    };
    let mut joint = base.clone();
    joint.participation = Participation::Everyone;
    let mut timer = base.clone();
    timer.goal = Goal::Numeric {
        minimum: Some(Decimal("1500".into())),
        maximum: None,
        unit: "seconds".into(),
    };
    let mut relative = base.clone();
    relative.schedule.repeat = Some(Cadence {
        frequency: Frequency::AfterCompletion,
        interval: 7,
    });
    relative.carry = Carry::RetainOne;
    Ok([
        ("daily", "Daily routine", base),
        ("chore", "Shared persistent chore", chore),
        ("weekly_quota", "Three times a week", quota),
        ("joint", "Joint daily routine", joint),
        ("focus_timer", "25 minutes of focus", timer),
        ("after_completion", "Seven days after completion", relative),
    ]
    .into_iter()
    .map(|(id, title, definition)| Preset {
        id: id.into(),
        title: title.into(),
        definition,
    })
    .collect())
}
