use crate::{
    error::{Error, SourceRange},
    format::CodeStr,
    line_index::LineIndex,
    parser,
    wiki::{PageTitle, Portal, SectionTitle, TITLE_PREFIX, Target, Wiki},
};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

// This is the own content of each page or section shown in portals as of the last time all of its
// instances agreed, which tells which instances have been changed since. A target without a
// baseline has no record of agreeing, so its instances must agree already.
pub type Baseline = HashMap<Target, Arc<str>>;

// These are the instances of each page or section shown in portals, with the original first, along
// with the copies which can't be read as content and so can't be compared with their originals.
#[derive(Debug, Default)]
pub struct Instances {
    targets: HashMap<Target, Vec<Instance>>,
    unreadable_copies: Vec<(Target, SourceRange)>,
}

// This is an instance of a page or a section shown in portals, which is either the original or a
// copy of it, possibly within another copy. Errors about it are reported at the original's title,
// or at the link in the copy's opening fence.
#[derive(Debug)]
struct Instance {
    content: String,
    source_range: SourceRange,
}

// This maps offsets in a copy read as a page back to the source of the wiki, through the copies
// containing it. Each line of the copy is listed with where it starts in the copy, after the title
// line which makes it a page, and where it starts in the source of the copy containing it.
struct CopyMap<'a> {
    title_line_length: usize,
    line_starts: Vec<(usize, usize)>,
    outer: Option<&'a CopyMap<'a>>,
}

impl CopyMap<'_> {
    // Map an offset after the title line to the source of the wiki.
    fn offset(&self, offset: usize) -> usize {
        let copy_offset = offset - self.title_line_length;
        let (copy_line_start, source_line_start) = self.line_starts[self
            .line_starts
            .partition_point(|(copy_line_start, _)| *copy_line_start <= copy_offset)
            - 1];
        let source_offset = source_line_start + copy_offset - copy_line_start;
        self.outer
            .map_or(source_offset, |copy_map| copy_map.offset(source_offset))
    }
}

// Collect the instances of each page or section shown in portals in a wiki without syntax errors.
pub fn instances(wiki: &Wiki) -> Instances {
    let mut instances = Instances::default();
    let mut pages = wiki.pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| page.source_range.start);
    for page in pages {
        for portal in &page.portals {
            collect_copies(wiki, wiki, None, portal, &mut instances);
        }
    }
    instances
}

// Record the copy a portal shows in a wiki, which is either the wiki itself or a copy read as a
// page with a map back to the wiki, and the copies within it, as instances of what they show. A
// copy of a missing page or section isn't an instance of anything, and a portal written on one line
// has no copy yet.
fn collect_copies(
    wiki: &Wiki,
    containing_wiki: &Wiki,
    copy_map: Option<&CopyMap>,
    portal: &Portal,
    instances: &mut Instances,
) {
    // Find what the portal shows, the copy, and where to report it.
    let Some((page, section)) = wiki.resolve(&portal.target) else {
        return;
    };
    let Some((copy, line_starts)) = containing_wiki.body(portal.source_range) else {
        return;
    };
    let source_range = copy_map.map_or(portal.link_source_range, |copy_map| SourceRange {
        start: copy_map.offset(portal.link_source_range.start),
        end: copy_map.offset(portal.link_source_range.end),
    });

    // Read the copy as the content of a page of its own, as deep as what it shows, which fails if a
    // line in it would start another page.
    let title_line = title_line(&page.title);
    let copy_source = format!("{title_line}{copy}");
    let (copy_wiki, errors) = parser::parse_at_level(
        None,
        &copy_source,
        &LineIndex::new(&copy_source),
        section.map_or(1, |section| section.level),
    );
    let Some(copy_page) = copy_wiki
        .pages
        .get(&page.title)
        .filter(|_| errors.is_empty() && copy_wiki.pages.len() == 1)
    else {
        instances
            .unreadable_copies
            .push((portal.target.clone(), source_range));
        return;
    };

    // Record the copy after the original, followed by the copies within it.
    instances
        .targets
        .entry(portal.target.clone())
        .or_insert_with(|| {
            vec![Instance {
                content: wiki.own_content(page, section),
                source_range: section.map_or(page.title_source_range, |section| {
                    section.title_source_range
                }),
            }]
        })
        .push(Instance {
            content: copy_wiki.own_content(copy_page, None),
            source_range,
        });
    let nested_copy_map = CopyMap {
        title_line_length: title_line.len(),
        line_starts,
        outer: copy_map,
    };
    for nested_portal in &copy_page.portals {
        collect_copies(
            wiki,
            &copy_wiki,
            Some(&nested_copy_map),
            nested_portal,
            instances,
        );
    }
}

// Write the title line which makes content the content of a page.
fn title_line(title: &PageTitle) -> String {
    format!(
        "{TITLE_PREFIX}{}\n\n",
        title.text(crate::escaping::Context::Verbatim),
    )
}

// List the own content of each page or section shown in portals whose instances all agree, which
// becomes its baseline.
pub fn agreements(instances: &Instances) -> impl Iterator<Item = (&Target, &str)> {
    let unreadable_targets = instances
        .unreadable_copies
        .iter()
        .map(|(target, _source_range)| target)
        .collect::<HashSet<_>>();
    instances
        .targets
        .iter()
        .filter(move |(target, _instances)| !unreadable_targets.contains(target))
        .filter(|(_target, target_instances)| {
            target_instances
                .iter()
                .all(|instance| instance.content == target_instances[0].content)
        })
        .map(|(target, target_instances)| (target, target_instances[0].content.as_str()))
}

// Format a wiki without syntax errors, propagating the changes to each page or section shown in
// portals to all of its instances.
pub fn format(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    instances: &Instances,
    baseline: &Baseline,
) -> Result<String, Vec<Error>> {
    // Render the rewritten wiki, which refreshes the copies from the pages.
    propagate(
        wiki,
        source_path,
        source_contents,
        line_index,
        instances,
        baseline,
    )
    .map(|propagated_wiki| propagated_wiki.to_string())
}

// Rewrite a wiki so each page or section shown in portals has the content to propagate to all of
// its instances: the content of the instances which differ from its baseline, if they all agree, or
// its own content if none do. Without a baseline, all of its instances must agree. A section's
// instances are within the instances of its page and of the sections containing it, so its changes
// are propagated to those before they're compared, unless they were changed there differently.
// Report the instances which were changed differently, changes which would create a cycle of
// portals, and copies which can't be compared with their originals.
#[allow(
    clippy::too_many_lines,
    reason = "Each step of propagation is described where the changes are found and written."
)]
pub fn propagate(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    instances: &Instances,
    baseline: &Baseline,
) -> Result<Wiki, Vec<Error>> {
    // Report errors at ranges of the source.
    let error = |message: &str, source_range| {
        Error::new(
            message,
            source_path,
            Some((source_contents, line_index, source_range)),
            None,
            None,
        )
    };
    let mismatch_message = |target: &Target| {
        format!(
            "{} and its copies don't match. Make them match, or replace a copy with {} to discard \
             its changes.",
            target.code_str(),
            format!("::: [{}] :::", target.link_text(false)).code_str(),
        )
    };

    // Report copies which can't be compared with their originals.
    let mut errors = instances
        .unreadable_copies
        .iter()
        .map(|(target, source_range)| {
            error(
                &format!(
                    "This copy of {} can't be read as the content of a page.",
                    target.code_str(),
                ),
                *source_range,
            )
        })
        .collect::<Vec<_>>();

    // Visit the targets in source order for deterministic errors, but visit each section before the
    // page and sections containing it, which come before it.
    let mut contents = instances
        .targets
        .iter()
        .map(|(target, target_instances)| {
            (
                target,
                target_instances
                    .iter()
                    .map(|instance| instance.content.clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut targets = instances
        .targets
        .keys()
        .map(|target| {
            let (page, section) = wiki.resolve(target).expect("Shown targets should exist.");
            (target, page, section)
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|(_target, page, section)| {
        (
            page.source_range.start,
            Reverse(section.map_or(page.source_range.start, |section| {
                section.source_range.start
            })),
        )
    });

    // Find the content to propagate for each target, skipping those which couldn't take the changes
    // of their sections, which have been reported.
    let mut changes = HashMap::new();
    let mut conflicting_targets = HashSet::new();
    for &(target, page, section) in &targets {
        if conflicting_targets.contains(target) {
            continue;
        }

        // Find the instances which have changed since the target's baseline, or all of them without
        // one.
        let target_baseline = baseline.get(target).map(AsRef::as_ref);
        let target_contents = &contents[target];
        let changed_instances = target_contents
            .iter()
            .enumerate()
            .filter(|(_index, content)| target_baseline.is_none_or(|baseline| baseline != *content))
            .map(|(index, _content)| index)
            .collect::<Vec<_>>();

        // Find the content of the changed instances if they agree, and report each of them
        // otherwise.
        let Some(&first_changed_instance) = changed_instances.first() else {
            continue;
        };
        let content = target_contents[first_changed_instance].clone();
        if changed_instances
            .iter()
            .any(|&index| target_contents[index] != content)
        {
            errors.extend(changed_instances.iter().map(|&index| {
                error(
                    &mismatch_message(target),
                    instances.targets[target][index].source_range,
                )
            }));
            continue;
        }

        // Write a section's content into the instances of the page and the sections containing it,
        // reporting those where it was changed differently or removed.
        if let Some(section) = section {
            for &(outer_target, _outer_page, outer_section) in &targets {
                if outer_target.title != target.title
                    || outer_section.is_some_and(|outer_section| {
                        outer_section.title == section.title
                            || !(outer_section.source_range.start..outer_section.source_range.end)
                                .contains(&section.source_range.start)
                    })
                {
                    continue;
                }
                let level = outer_section.map_or(1, |outer_section| outer_section.level);
                for (index, outer_content) in contents
                    .get_mut(outer_target)
                    .expect("Each target should have contents.")
                    .iter_mut()
                    .enumerate()
                {
                    if let Some(new_content) = write_section(
                        outer_content,
                        &page.title,
                        level,
                        &section.title,
                        &content,
                        target_baseline,
                    ) {
                        *outer_content = new_content;
                    } else {
                        errors.push(error(
                            &mismatch_message(target),
                            instances.targets[outer_target][index].source_range,
                        ));
                        conflicting_targets.insert(outer_target);
                    }
                }
            }
        }
        changes.insert(
            target,
            (
                content,
                changed_instances
                    .iter()
                    .map(|&index| instances.targets[target][index].source_range)
                    .collect::<Vec<_>>(),
            ),
        );
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Find the new content of each page with changes: its own changes, which include those of its
    // sections, or else its own content with the changes of its sections. Replace the content of
    // each page whose content changes, from the end so earlier offsets stay valid, and read the
    // result.
    let mut section_changes = HashMap::<&PageTitle, HashMap<SectionTitle, String>>::new();
    for (target, (content, _source_ranges)) in &changes {
        if let Some(section) = &target.section {
            section_changes
                .entry(&target.title)
                .or_default()
                .insert(section.clone(), content.clone());
        }
    }
    let mut replacements = changes
        .keys()
        .map(|target| &target.title)
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|title| {
            let page = &wiki.pages[title];
            let content = changes.get(&page.target(None)).map_or_else(
                || wiki.own_content_with(page, None, &section_changes[title]),
                |(content, _source_ranges)| content.clone(),
            );
            (content != wiki.own_content(page, None)).then_some((page, content))
        })
        .collect::<Vec<_>>();
    replacements.sort_by_key(|(page, _content)| Reverse(page.source_range.start));
    let mut propagated_contents = source_contents.to_owned();
    for (page, content) in &replacements {
        propagated_contents.replace_range(
            page.title_source_range.end..page.source_range.end,
            &format!("\n\n{content}"),
        );
    }
    let propagated_line_index = LineIndex::new(&propagated_contents);
    let (propagated_wiki, syntax_errors) =
        parser::parse(source_path, &propagated_contents, &propagated_line_index);
    if !syntax_errors.is_empty() {
        return Err(syntax_errors);
    }

    // Report the changed instances of each target which would create a cycle of portals once its
    // changes were propagated. Its copies couldn't be filled in.
    for (target, (_content, source_ranges)) in &changes {
        if !propagated_wiki
            .cycle_closing_portals([(*target).clone()])
            .is_empty()
        {
            errors.extend(source_ranges.iter().map(|source_range| {
                error(
                    "Propagating this change would create a cycle of portals.",
                    *source_range,
                )
            }));
        }
    }
    if errors.is_empty() {
        Ok(propagated_wiki)
    } else {
        errors.sort_by_key(|error| error.source_range().map(|source_range| source_range.start));
        Err(errors)
    }
}

// Write a section's propagated content into an instance of the page or a section containing it, at
// a level, unless the instance doesn't have the section or has changed it since its baseline, or at
// all without one.
fn write_section(
    instance: &str,
    title: &PageTitle,
    level: usize,
    section_title: &SectionTitle,
    content: &str,
    baseline: Option<&str>,
) -> Option<String> {
    let source = format!("{}{instance}", title_line(title));
    let (instance_wiki, errors) =
        parser::parse_at_level(None, &source, &LineIndex::new(&source), level);
    let page = instance_wiki
        .pages
        .get(title)
        .filter(|_| errors.is_empty())?;
    let section_content = instance_wiki.own_content(page, Some(page.section(section_title)?));
    (section_content == content || baseline.is_some_and(|baseline| section_content == baseline))
        .then(|| {
            instance_wiki.own_content_with(
                page,
                None,
                &HashMap::from([(section_title.clone(), content.to_owned())]),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{Baseline, agreements, format, instances};
    use crate::{
        assert_fails,
        error::Error,
        line_index::LineIndex,
        parser,
        wiki::{PageTitle, SectionTitle, Target},
    };
    use std::{fmt::Write, sync::Arc};

    // Format a wiki without syntax errors, given the baseline of each page.
    fn format_test(source: &str, baseline: &[(&str, &str)]) -> Result<String, Vec<Error>> {
        let line_index = LineIndex::new(source);
        let (wiki, errors) = parser::parse(None, source, &line_index);
        assert!(errors.is_empty());
        let baseline = baseline
            .iter()
            .map(|(target, content)| {
                let (title, section) = target
                    .split_once('#')
                    .map_or((*target, None), |(title, section)| {
                        (title, Some(SectionTitle::parse(section).unwrap()))
                    });
                (
                    Target {
                        title: PageTitle::parse(title).unwrap(),
                        section,
                    },
                    Arc::from(*content),
                )
            })
            .collect::<Baseline>();
        format(
            &wiki,
            None,
            source,
            &line_index,
            &instances(&wiki),
            &baseline,
        )
    }

    // Find the text each error is reported at.
    fn error_texts<'a>(source: &'a str, errors: &[Error]) -> Vec<&'a str> {
        errors
            .iter()
            .map(|error| {
                let source_range = error.source_range().unwrap();
                &source[source_range.start..source_range.end]
            })
            .collect()
    }

    // Compare instances as formatting would write them, so differences it erases aren't changes.
    #[test]
    fn canonical_instances() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n  ::: [Pancakes]\n  Mix [ Home ].  \r\n\n  :::\n\n",
                    "# Pancakes\n\nMix [Home].",
                ),
                &[],
            )
            .unwrap(),
            "# Home\n\n  ::: [Pancakes]\n  Mix [Home].\n  :::\n\n# Pancakes\n\nMix [Home].\n",
        );
    }

    // Treat a copy within another copy as an instance of the page it shows, but not as part of the
    // instance containing it.
    #[test]
    fn nested_copies() {
        let source = concat!(
            "# Home\n\n::: [Pancakes]\nMix.\n\n  ::: [Syrup]\n  Pour lots.\n  :::\n:::\n\n",
            "# Pancakes\n\nMix.\n\n  ::: [Syrup]\n  Pour.\n  :::\n\n",
            "# Syrup\n\nPour.\n",
        );

        assert_eq!(
            format_test(
                source,
                &[
                    ("Pancakes", "Mix.\n\n  ::: [Syrup]\n  :::"),
                    ("Syrup", "Pour."),
                ],
            )
            .unwrap(),
            concat!(
                "# Home\n\n:::: [Pancakes]\nMix.\n\n  ::: [Syrup]\n  Pour lots.\n  :::\n::::\n\n",
                "# Pancakes\n\nMix.\n\n  ::: [Syrup]\n  Pour lots.\n  :::\n\n",
                "# Syrup\n\nPour lots.\n",
            ),
        );
        assert_eq!(
            error_texts(source, &format_test(source, &[]).unwrap_err()),
            vec!["Syrup", "[Syrup]", "[Syrup]"],
        );
    }

    // Fill in a portal written on one line, which isn't an instance until then.
    #[test]
    fn one_line_portals() {
        assert_eq!(
            format_test("# Home\n\n::: [Pancakes] :::\n\n# Pancakes\n\nMix.\n", &[]).unwrap(),
            "# Home\n\n::: [Pancakes]\nMix.\n:::\n\n# Pancakes\n\nMix.\n",
        );
    }

    // Report a copy with a line which would start another page if it were a page's content.
    #[test]
    fn unreadable_copies() {
        let source = "# Home\n\n  ::: [Pancakes]\n  # Mix.\n  :::\n\n# Pancakes\n\nMix.\n";
        let errors = format_test(source, &[("Pancakes", "Mix.")]).unwrap_err();

        assert_eq!(error_texts(source, &errors), vec!["[Pancakes]"]);
        assert_fails!(
            Err::<(), _>(errors),
            "This copy of `Pancakes` can't be read as the content of a page.",
        );
    }

    // List the own content of the pages whose instances agree.
    #[test]
    fn agreeing_instances() {
        let source = concat!(
            "# Home\n\n::: [Pancakes]\nMix.\n:::\n\n::: [Syrup]\nPour lots.\n:::\n\n",
            "# Pancakes\n\nMix.\n\n# Syrup\n\nPour.\n",
        );
        let (wiki, errors) = parser::parse(None, source, &LineIndex::new(source));
        assert!(errors.is_empty());
        let instances = instances(&wiki);

        assert_eq!(
            agreements(&instances)
                .map(|(target, content)| (target.link_text(false), content))
                .collect::<Vec<_>>(),
            vec![("Pancakes".to_owned(), "Mix.")],
        );
    }

    // Propagate a change to a page to its copies.
    #[test]
    fn page_changes() {
        assert_eq!(
            format_test(
                "# Home\n\n::: [Pancakes]\nMix.\n:::\n\n# Pancakes\n\nMix well.\n",
                &[("Pancakes", "Mix.")],
            )
            .unwrap(),
            "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n# Pancakes\n\nMix well.\n",
        );
    }

    // Propagate a change to a copy to its page and the other copies.
    #[test]
    fn copy_changes() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n[Other]\n\n",
                    "# Other\n\n::: [Pancakes]\nMix.\n:::\n\n",
                    "# Pancakes\n\nMix.\n",
                ),
                &[("Pancakes", "Mix.")],
            )
            .unwrap(),
            concat!(
                "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n[Other]\n\n",
                "# Other\n\n::: [Pancakes]\nMix well.\n:::\n\n",
                "# Pancakes\n\nMix well.\n",
            ),
        );
    }

    // Propagate the same change made to several instances.
    #[test]
    fn identical_changes() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n",
                    "::: [Pancakes]\nMix well.\n:::\n\n",
                    "# Pancakes\n\nMix.\n",
                ),
                &[("Pancakes", "Mix.")],
            )
            .unwrap(),
            concat!(
                "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n",
                "::: [Pancakes]\nMix well.\n:::\n\n",
                "# Pancakes\n\nMix well.\n",
            ),
        );
    }

    // Propagate a copy which was emptied like any other change.
    #[test]
    fn emptied_copies() {
        assert_eq!(
            format_test(
                "# Home\n\n::: [Pancakes]\n:::\n\n# Pancakes\n\nMix.\n",
                &[("Pancakes", "Mix.")],
            )
            .unwrap(),
            "# Home\n\n::: [Pancakes]\n:::\n\n# Pancakes\n",
        );
    }

    // Report each instance changed differently from the others, rather than choosing one.
    #[test]
    fn different_changes() {
        let source = concat!(
            "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n",
            "::: [Pancakes]\nMix.\n:::\n\n",
            "# Pancakes\n\nMix quickly.\n",
        );
        let errors = format_test(source, &[("Pancakes", "Mix.")]).unwrap_err();

        assert_eq!(error_texts(source, &errors), vec!["Pancakes", "[Pancakes]"]);
        assert_fails!(
            Err::<(), _>(errors),
            "`Pancakes` and its copies don't match.",
        );
    }

    // Require the instances of a page without a baseline to match, reporting all of them otherwise.
    #[test]
    fn instances_without_baseline() {
        let source = "# Home\n\n::: [Pancakes]\nMix well.\n:::\n\n# Pancakes\n\nMix.\n";
        let errors = format_test(source, &[]).unwrap_err();

        assert_eq!(error_texts(source, &errors), vec!["Pancakes", "[Pancakes]"]);
        assert_fails!(
            Err::<(), _>(errors),
            "`Pancakes` and its copies don't match.",
        );
        assert!(
            format_test(
                "# Home\n\n::: [Pancakes]\nMix.\n:::\n\n# Pancakes\n\nMix.",
                &[],
            )
            .is_ok(),
        );
    }

    // Report a change which would create a cycle of portals, putting a portal to a page inside
    // itself, directly or through other pages.
    #[test]
    fn changes_which_put_pages_in_themselves() {
        for shown_title in ["Pancakes", "Syrup"] {
            let source = format!(
                concat!(
                    "# Home\n\n::: [Pancakes]\n::: [{}] :::\n\nMix.\n:::\n\n",
                    "# Pancakes\n\nMix.\n\n# Syrup\n\n::: [Pancakes]\nMix.\n:::\n",
                ),
                shown_title,
            );
            let errors = format_test(&source, &[("Pancakes", "Mix.")]).unwrap_err();

            assert_eq!(error_texts(&source, &errors), vec!["[Pancakes]"]);
            assert_fails!(
                Err::<(), _>(errors),
                "Propagating this change would create a cycle of portals.",
            );
        }
    }

    // Propagate a change to a copy of a section to the section in its page and to the copies of the
    // page, and fill in a portal to a section written on one line.
    #[test]
    fn section_copy_changes() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n::: [Budget#Monthly]\nRent: 2100\n:::\n\n",
                    "::: [Budget]\nIntro.\n\n## Monthly\n\nRent: 2000\n:::\n\n",
                    "::: [#Notes] :::\n\n## Notes\n\nNone.\n\n",
                    "# Budget\n\nIntro.\n\n## Monthly\n\nRent: 2000\n",
                ),
                &[
                    ("Budget", "Intro.\n\n## Monthly\n\nRent: 2000"),
                    ("Budget#Monthly", "Rent: 2000"),
                ],
            )
            .unwrap(),
            concat!(
                "# Home\n\n::: [Budget#Monthly]\nRent: 2100\n:::\n\n",
                "::: [Budget]\nIntro.\n\n## Monthly\n\nRent: 2100\n:::\n\n",
                "::: [#Notes]\nNone.\n:::\n\n## Notes\n\nNone.\n\n",
                "# Budget\n\nIntro.\n\n## Monthly\n\nRent: 2100\n",
            ),
        );
    }

    // Propagate a change to a section in its page to its copies, and to the copies of the page.
    #[test]
    fn section_changes() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n::: [Budget#Monthly]\nRent: 2000\n:::\n\n",
                    "::: [Budget]\nIntro.\n\n## Monthly\n\nRent: 2000\n:::\n\n",
                    "# Budget\n\nIntro.\n\n## Monthly\n\nRent: 2100\n",
                ),
                &[
                    ("Budget", "Intro.\n\n## Monthly\n\nRent: 2000"),
                    ("Budget#Monthly", "Rent: 2000"),
                ],
            )
            .unwrap(),
            concat!(
                "# Home\n\n::: [Budget#Monthly]\nRent: 2100\n:::\n\n",
                "::: [Budget]\nIntro.\n\n## Monthly\n\nRent: 2100\n:::\n\n",
                "# Budget\n\nIntro.\n\n## Monthly\n\nRent: 2100\n",
            ),
        );
    }

    // Propagate changes to copies of fenced sections nested in each other, writing each inner
    // change into the instances of the sections containing it.
    #[test]
    fn nested_section_changes() {
        assert_eq!(
            format_test(
                concat!(
                    "# Home\n\n::: [Budget#Fixed]\nRent: 2000\n\n### Utilities\n\n",
                    "Power: 90\n:::\n\n::: [Budget#Utilities]\nPower: 90\n:::\n\n",
                    "# Budget\n\n:::: ## Fixed\nRent: 2000\n\n### Utilities\n\nPower: 80\n::::\n",
                ),
                &[
                    ("Budget#Fixed", "Rent: 2000\n\n### Utilities\n\nPower: 80"),
                    ("Budget#Utilities", "Power: 80"),
                ],
            )
            .unwrap(),
            concat!(
                "# Home\n\n::: [Budget#Fixed]\nRent: 2000\n\n### Utilities\n\nPower: 90\n:::\n\n",
                "::: [Budget#Utilities]\nPower: 90\n:::\n\n",
                "# Budget\n\n::: ## Fixed\nRent: 2000\n\n### Utilities\n\nPower: 90\n:::\n",
            ),
        );
    }

    // Report a section changed differently in a copy of it and in a copy of its page, and a page
    // changed in a copy of it while one of its sections is changed in another.
    #[test]
    fn conflicting_section_changes() {
        let source = concat!(
            "# Home\n\n::: [Budget#Monthly]\nRent: 2100\n:::\n\n",
            "::: [Budget]\nIntro.\n\n## Monthly\n\nRent: 2200\n:::\n\n",
            "# Budget\n\nIntro.\n\n## Monthly\n\nRent: 2000\n",
        );
        let baseline = [
            ("Budget", "Intro.\n\n## Monthly\n\nRent: 2000"),
            ("Budget#Monthly", "Rent: 2000"),
        ];
        let errors = format_test(source, &baseline).unwrap_err();
        assert_eq!(error_texts(source, &errors), vec!["[Budget]"]);
        assert_fails!(
            Err::<(), _>(errors),
            "`Budget#Monthly` and its copies don't match.",
        );

        let source = source.replace(
            "Intro.\n\n## Monthly\n\nRent: 2200",
            "More.\n\n## Monthly\n\nRent: 2000",
        );
        let errors = format_test(&source, &baseline).unwrap_err();
        assert_eq!(error_texts(&source, &errors), vec!["Budget", "[Budget]"]);
        assert_fails!(Err::<(), _>(errors), "`Budget` and its copies don't match.");
    }
}
