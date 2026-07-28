use chrono::{Datelike, NaiveDate};
use std::collections::{HashMap, HashSet};

/// Service calendar: which service_ids run on a given date.
#[derive(Debug, Clone, Default)]
pub struct ServiceCalendar {
    /// service_id -> (start, end, weekday bitmask mon=1<<0 ... sun=1<<6)
    regular: HashMap<String, RegularService>,
    /// date -> added service_ids
    added: HashMap<NaiveDate, HashSet<String>>,
    /// date -> removed service_ids
    removed: HashMap<NaiveDate, HashSet<String>>,
}

#[derive(Debug, Clone)]
struct RegularService {
    start: NaiveDate,
    end: NaiveDate,
    /// bit 0 = Monday ... bit 6 = Sunday
    days: u8,
}

impl ServiceCalendar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_regular(
        &mut self,
        service_id: String,
        start: NaiveDate,
        end: NaiveDate,
        mon: bool,
        tue: bool,
        wed: bool,
        thu: bool,
        fri: bool,
        sat: bool,
        sun: bool,
    ) {
        let mut days = 0u8;
        if mon {
            days |= 1 << 0;
        }
        if tue {
            days |= 1 << 1;
        }
        if wed {
            days |= 1 << 2;
        }
        if thu {
            days |= 1 << 3;
        }
        if fri {
            days |= 1 << 4;
        }
        if sat {
            days |= 1 << 5;
        }
        if sun {
            days |= 1 << 6;
        }
        self.regular
            .insert(service_id, RegularService { start, end, days });
    }

    pub fn add_exception(&mut self, service_id: String, date: NaiveDate, exception_type: u8) {
        match exception_type {
            1 => {
                self.added.entry(date).or_default().insert(service_id);
            }
            2 => {
                self.removed.entry(date).or_default().insert(service_id);
            }
            _ => {}
        }
    }

    pub fn is_active(&self, service_id: &str, date: NaiveDate) -> bool {
        if self
            .removed
            .get(&date)
            .map(|s| s.contains(service_id))
            .unwrap_or(false)
        {
            return false;
        }
        if self
            .added
            .get(&date)
            .map(|s| s.contains(service_id))
            .unwrap_or(false)
        {
            return true;
        }
        if let Some(reg) = self.regular.get(service_id) {
            if date < reg.start || date > reg.end {
                return false;
            }
            let weekday = date.weekday().num_days_from_monday() as u8; // 0=Mon
            return (reg.days & (1 << weekday)) != 0;
        }
        false
    }

    pub fn active_services(&self, date: NaiveDate) -> HashSet<String> {
        let mut out = HashSet::new();
        for sid in self.regular.keys() {
            if self.is_active(sid, date) {
                out.insert(sid.clone());
            }
        }
        if let Some(added) = self.added.get(&date) {
            for sid in added {
                if !self
                    .removed
                    .get(&date)
                    .map(|r| r.contains(sid))
                    .unwrap_or(false)
                {
                    out.insert(sid.clone());
                }
            }
        }
        out
    }

    /// Union of service_ids active on any day in `[start, end]` (inclusive).
    pub fn services_active_in_range(&self, start: NaiveDate, end: NaiveDate) -> HashSet<String> {
        let mut out = HashSet::new();
        if end < start {
            return out;
        }
        let mut d = start;
        loop {
            out.extend(self.active_services(d));
            if d >= end {
                break;
            }
            match d.succ_opt() {
                Some(n) => d = n,
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn weekday_service() {
        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        cal.add_regular(
            "S1".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            false,
            false,
        );
        // 2026-07-27 is Monday
        let mon = NaiveDate::from_ymd_opt(2026, 7, 27).unwrap();
        assert!(cal.is_active("S1", mon));
        let sat = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        assert!(!cal.is_active("S1", sat));
    }

    #[test]
    fn services_active_in_range_union() {
        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        // weekdays only
        cal.add_regular(
            "WD".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            false,
            false,
        );
        // weekend only
        cal.add_regular(
            "WE".into(),
            start,
            end,
            false,
            false,
            false,
            false,
            false,
            true,
            true,
        );
        // Mon 2026-07-27 .. Sun 2026-08-02
        let from = NaiveDate::from_ymd_opt(2026, 7, 27).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 8, 2).unwrap();
        let set = cal.services_active_in_range(from, to);
        assert!(set.contains("WD"));
        assert!(set.contains("WE"));
        // Mon only window → no weekend service
        let mon_only = cal.services_active_in_range(from, from);
        assert!(mon_only.contains("WD"));
        assert!(!mon_only.contains("WE"));
    }
}
