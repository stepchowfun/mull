use crate::{
    error::{Error, SourceRange},
    escaping::Context,
    format::CodeStr,
    line_index::LineIndex,
    parser,
    wiki::{Embed, PageTitle, TITLE_PREFIX, Wiki},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

// This is the own content of each embedded page as of the last time all of its instances agreed,
// which tells which instances have been changed since. A page without a baseline has no record of
// agreeing, so its instances must agree already.
pub type Baseline = HashMap<PageTitle, Arc<str>>;

// This is an instance of an embedded page, which is either the page itself or a copy of it,
// possibly within another copy. Errors about it are reported at the page's title or at the link in
// the copy's opening fence.
#[derive(Debug)]
struct Instance {
    content: String,
    source_range: SourceRange,
}

// These are the instances of each embedded page, with the page itself first, along with the copies
// which can't be read as a page's content and so can't be compared with their pages.
#[derive(Debug, Default)]
pub struct Instances {
    pages: HashMap<PageTitle, Vec<Instance>>,
    unreadable_copies: Vec<(PageTitle, SourceRange)>,
}

// Collect the instances of each embedded page in a wiki without syntax errors.
pub fn instances(wiki: &Wiki) -> Instances {
    let mut instances = Instances::default();
    let mut pages = wiki.pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| page.source_range.start);
    for page in pages {
        for embed in &page.embeds {
            collect_copies(wiki, wiki, None, embed, &mut instances);
        }
    }
    instances
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

// Record the copy an embed shows in a wiki, which is either the wiki itself or a copy read as a
// page with a map back to the wiki, and the copies within it, as instances of the pages they embed.
// A copy of a missing page isn't an instance of anything, and an embed written on one line has no
// copy yet.
fn collect_copies(
    wiki: &Wiki,
    embedding_wiki: &Wiki,
    copy_map: Option<&CopyMap>,
    embed: &Embed,
    instances: &mut Instances,
) {
    // Find the embedded page, the copy, and where to report it.
    let Some(page) = wiki.pages.get(&embed.title) else {
        return;
    };
    let Some((copy, line_starts)) = embedding_wiki.copy(embed) else {
        return;
    };
    let source_range = copy_map.map_or(embed.link_source_range, |copy_map| SourceRange {
        start: copy_map.offset(embed.link_source_range.start),
        end: copy_map.offset(embed.link_source_range.end),
    });

    // Read the copy as the content of a page of its own, which fails if a line in it would start
    // another page.
    let title_line = format!("{TITLE_PREFIX}{}\n\n", embed.title.text(Context::Verbatim));
    let copy_source = format!("{title_line}{copy}");
    let (copy_wiki, errors) = parser::parse(None, &copy_source, &LineIndex::new(&copy_source));
    let Some(copy_page) = copy_wiki
        .pages
        .get(&embed.title)
        .filter(|_| errors.is_empty() && copy_wiki.pages.len() == 1)
    else {
        instances
            .unreadable_copies
            .push((embed.title.clone(), source_range));
        return;
    };

    // Record the copy after the page itself, followed by the copies within it.
    instances
        .pages
        .entry(embed.title.clone())
        .or_insert_with(|| {
            vec![Instance {
                content: wiki.own_content(page),
                source_range: page.title_source_range,
            }]
        })
        .push(Instance {
            content: copy_wiki.own_content(copy_page),
            source_range,
        });
    let nested_copy_map = CopyMap {
        title_line_length: title_line.len(),
        line_starts,
        outer: copy_map,
    };
    for nested_embed in &copy_page.embeds {
        collect_copies(
            wiki,
            &copy_wiki,
            Some(&nested_copy_map),
            nested_embed,
            instances,
        );
    }
}

// List the own content of each embedded page whose instances all agree, which becomes its
// baseline.
pub fn agreements(instances: &Instances) -> impl Iterator<Item = (&PageTitle, &str)> {
    let unreadable_titles = instances
        .unreadable_copies
        .iter()
        .map(|(title, _source_range)| title)
        .collect::<HashSet<_>>();
    instances
        .pages
        .iter()
        .filter(move |(title, _instances)| !unreadable_titles.contains(title))
        .filter(|(_title, page_instances)| {
            page_instances
                .iter()
                .all(|instance| instance.content == page_instances[0].content)
        })
        .map(|(title, page_instances)| (title, page_instances[0].content.as_str()))
}

// Rewrite the source so each embedded page has the content to propagate to all of its instances:
// the content of the instances which differ from its baseline, if they all agree, or its own
// content if none do. Without a baseline, all of its instances must agree. Report the instances
// which were changed differently, and copies which can't be compared with their pages.
pub fn propagate(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    instances: &Instances,
    baseline: &Baseline,
) -> Result<String, Vec<Error>> {
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

    // Report copies which can't be compared with their pages.
    let mut errors = instances
        .unreadable_copies
        .iter()
        .map(|(title, source_range)| {
            error(
                &format!(
                    "This copy of {} can't be read as the content of a page.",
                    title.code_str(),
                ),
                *source_range,
            )
        })
        .collect::<Vec<_>>();

    // Find the content to propagate for each page, in source order for deterministic errors.
    let mut titles = instances.pages.keys().collect::<Vec<_>>();
    titles.sort_by_key(|title| wiki.pages[*title].source_range.start);
    let mut replacements = Vec::new();
    for title in titles {
        // Find the instances which have changed since the page's baseline, or all of them without
        // one.
        let page_instances = &instances.pages[title];
        let changed_instances = match baseline.get(title) {
            Some(content) => page_instances
                .iter()
                .filter(|instance| *instance.content != **content)
                .collect::<Vec<_>>(),
            None => page_instances.iter().collect(),
        };

        // Propagate the changed instances if they agree, and report each of them otherwise.
        let Some(first_changed_instance) = changed_instances.first() else {
            continue;
        };
        if changed_instances
            .iter()
            .all(|instance| instance.content == first_changed_instance.content)
        {
            if first_changed_instance.content != page_instances[0].content {
                replacements.push((&wiki.pages[title], &first_changed_instance.content));
            }
        } else {
            let message = format!(
                "{} and its copies don't match. Make them match, or replace a copy with {} to \
                 discard its changes.",
                title.code_str(),
                format!("::: [{}] :::", title.text(Context::Link)).code_str(),
            );
            errors.extend(
                changed_instances
                    .iter()
                    .map(|instance| error(&message, instance.source_range)),
            );
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Replace the content of each page to propagate, from the end so earlier offsets stay valid.
    replacements.sort_by_key(|(page, _content)| std::cmp::Reverse(page.source_range.start));
    let mut propagated_contents = source_contents.to_owned();
    for (page, content) in replacements {
        propagated_contents.replace_range(
            page.title_source_range.end..page.source_range.end,
            &format!("\n\n{content}"),
        );
    }
    Ok(propagated_contents)
}

// Format a wiki without syntax errors, propagating the changes to each embedded page to all of its
// instances.
pub fn format(
    wiki: &Wiki,
    source_path: Option<&Path>,
    source_contents: &str,
    line_index: &LineIndex,
    instances: &Instances,
    baseline: &Baseline,
) -> Result<String, Vec<Error>> {
    // Render the rewritten source, which refreshes the copies from the pages.
    let propagated_contents = propagate(
        wiki,
        source_path,
        source_contents,
        line_index,
        instances,
        baseline,
    )?;
    let propagated_line_index = LineIndex::new(&propagated_contents);
    let (propagated_wiki, errors) =
        parser::parse(source_path, &propagated_contents, &propagated_line_index);
    if errors.is_empty() {
        Ok(propagated_wiki.to_string())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{Baseline, agreements, format, instances};
    use crate::{assert_fails, error::Error, line_index::LineIndex, parser, wiki::PageTitle};
    use std::{fmt::Write, sync::Arc};

    // Format a wiki without syntax errors, given the baseline of each page.
    fn format_test(source: &str, baseline: &[(&str, &str)]) -> Result<String, Vec<Error>> {
        let line_index = LineIndex::new(source);
        let (wiki, errors) = parser::parse(None, source, &line_index);
        assert!(errors.is_empty());
        let baseline = baseline
            .iter()
            .map(|(title, content)| (PageTitle::parse(title).unwrap(), Arc::from(*content)))
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
                &[]
            )
            .is_ok()
        );
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

    // Treat a copy within another copy as an instance of the page it embeds, but not as part of the
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
                    ("Syrup", "Pour.")
                ]
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

    // Fill in an embed written on one line, which isn't an instance until then.
    #[test]
    fn one_line_embeds() {
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
                .map(|(title, content)| (title.text(crate::escaping::Context::Verbatim), content))
                .collect::<Vec<_>>(),
            vec![("Pancakes".to_owned(), "Mix.")],
        );
    }
}
