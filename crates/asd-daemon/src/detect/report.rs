//! One evaluator feeds both the compact verdict and the structured explanation.

use asd_proto::{DetectionReport, DetectionRuleReport};

use super::{Detector, Manifest, Region, RegionText, Rule, Screen, agent_id};

impl Detector {
    pub fn explain(&self, generation: u64, command: &str, screen: &Screen<'_>) -> DetectionReport {
        let mut rules = Vec::new();
        let (candidates, winner) =
            self.evaluate(command, screen, |manifest, rule, lines, result| {
                let (evidence, reason) = match result {
                    Ok(indexes) => (indexes.iter().map(|&i| lines[i].clone()).collect(), None),
                    Err(reason) => (Vec::new(), Some(reason.clone())),
                };
                rules.push(DetectionRuleReport {
                    manifest_id: manifest.id.clone(),
                    rule_id: rule.id.clone(),
                    priority: rule.priority,
                    state: rule.state,
                    region: rule.region.to_string(),
                    matched: result.is_ok(),
                    evidence,
                    reason,
                });
            });
        DetectionReport {
            foreground_command: command.into(),
            selected_manifest_id: candidates.first().map(|manifest| manifest.id.clone()),
            candidate_manifest_ids: candidates
                .iter()
                .map(|manifest| manifest.id.clone())
                .collect(),
            state: winner.map(|rule| rule.state).unwrap_or_default(),
            rules,
            generation,
        }
    }

    pub(super) fn evaluate(
        &self,
        command: &str,
        screen: &Screen<'_>,
        mut visit: impl FnMut(&Manifest, &Rule, &[String], &Result<Vec<usize>, String>),
    ) -> (Vec<&Manifest>, Option<&Rule>) {
        let Some(id) = agent_id(command) else {
            return (Vec::new(), None);
        };
        let candidates: Vec<_> = self
            .manifests
            .iter()
            .filter(|manifest| manifest.matches_agent(&id))
            .collect();
        let Some(manifest) = candidates.first() else {
            return (candidates, None);
        };
        // Stable sorting preserves the file-order tie break.
        let mut rules: Vec<_> = manifest.rules.iter().collect();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        let mut cache: Vec<(Region, Vec<String>, RegionText)> = Vec::new();
        let mut winner = None;
        for rule in rules {
            let index = match cache
                .iter()
                .position(|(region, _, _)| *region == rule.region)
            {
                Some(index) => index,
                None => {
                    let lines = screen.region(rule.region);
                    let text = RegionText::new(lines.clone());
                    cache.push((rule.region, lines, text));
                    cache.len() - 1
                }
            };
            let (_, lines, text) = &cache[index];
            let result = rule.predicate.evaluate(text);
            if winner.is_none() && result.is_ok() {
                winner = Some(rule);
            }
            visit(manifest, rule, lines, &result);
        }
        (candidates, winner)
    }
}
