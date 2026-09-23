use std::collections::HashSet;

use moli_layout::{
    LayoutAnswers, LayoutBoxModel, LayoutCaretPosition, LayoutDocumentMetrics,
    LayoutElementMetrics, LayoutError, LayoutFlushReason, LayoutHit, LayoutPoint, LayoutQuery,
    LayoutQueryAnswer, LayoutQueryBatch, LayoutRect, LayoutResolvedGridTracks,
    LayoutScrollContainerMetrics, LayoutScrollIntoViewGeometry, LayoutTransform2D,
};

use super::client_rect::{ClientRect, client_rect_from_quad, union_client_rect, zero_client_rect};
use super::mock::{
    answer_queries as answer_mock_queries, compute_mock_scroll_adjusted_client_rect,
};
use super::scroll::node_scroll_position;
use crate::{document_runtime::DomHandle, native_bridge::JsContextHost};

pub(crate) fn observable_geometry_batch(
    runtime: &JsContextHost,
    document: DomHandle,
    reason: LayoutFlushReason,
    batch: &LayoutQueryBatch<DomHandle>,
) -> Result<LayoutAnswers<DomHandle>, LayoutError> {
    if runtime.layout_policy().uses_real_layout() {
        runtime.answer_layout_for_document(document, reason, batch)
    } else {
        Ok(answer_mock_queries(runtime, document, reason, batch))
    }
}

pub(crate) fn observable_client_rects(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<Vec<ClientRect>, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(Vec::new());
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(Vec::new());
    };
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::ClientRects { source }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::ClientRects(rects)) => {
            Ok(rects.into_iter().map(client_rect_from_quad).collect())
        }
        _ => Err(provider_contract_error("client rects")),
    }
}

pub(crate) fn observable_sources_with_fragments(
    runtime: &JsContextHost,
    document: DomHandle,
    sources: &[DomHandle],
    reason: LayoutFlushReason,
) -> Result<HashSet<DomHandle>, LayoutError> {
    if sources.is_empty() {
        return Ok(HashSet::new());
    }
    let queries = sources
        .iter()
        .copied()
        .map(|source| LayoutQuery::ContentQuads { source })
        .collect();
    let answers =
        observable_geometry_batch(runtime, document, reason, &LayoutQueryBatch::new(queries))?;
    if answers.answers.len() != sources.len() {
        return Err(provider_contract_error("rendered source fragment"));
    }
    let mut rendered = HashSet::new();
    for (source, answer) in sources.iter().copied().zip(answers.answers) {
        match answer {
            LayoutQueryAnswer::ContentQuads(quads) => {
                if !quads.is_empty() {
                    rendered.insert(source);
                }
            }
            _ => return Err(provider_contract_error("rendered source fragment")),
        }
    }
    Ok(rendered)
}

pub(crate) fn observable_bounding_client_rect(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<ClientRect, LayoutError> {
    let mut rects = observable_client_rects(runtime, source, reason)?.into_iter();
    let Some(mut bounds) = rects.next() else {
        return Ok(zero_client_rect());
    };
    for rect in rects {
        bounds = union_client_rect(bounds, rect);
    }
    Ok(bounds)
}

pub(crate) fn observable_bounding_client_rects(
    runtime: &JsContextHost,
    sources: &[DomHandle],
    reason: LayoutFlushReason,
) -> Result<Vec<ClientRect>, LayoutError> {
    let Some((&first, rest)) = sources.split_first() else {
        return Ok(Vec::new());
    };
    if !runtime.dom_host().is_connected(first) {
        return Ok(vec![zero_client_rect(); sources.len()]);
    }
    let Some(document) = runtime.layout_document_for_source(first) else {
        return Ok(vec![zero_client_rect(); sources.len()]);
    };
    if rest.iter().any(|source| {
        runtime.layout_document_for_source(*source) != Some(document)
            || !runtime.dom_host().is_connected(*source)
    }) {
        return Err(LayoutError::source_contract(
            "geometry batch",
            "bounding-client-rect sources do not share one connected document",
        ));
    }
    let queries = sources
        .iter()
        .copied()
        .map(|source| LayoutQuery::ClientRects { source })
        .collect();
    let answers =
        observable_geometry_batch(runtime, document, reason, &LayoutQueryBatch::new(queries))?;
    if answers.answers.len() != sources.len() {
        return Err(provider_contract_error("bounding client rects"));
    }
    answers
        .answers
        .into_iter()
        .map(|answer| match answer {
            LayoutQueryAnswer::ClientRects(rects) => Ok(rects
                .into_iter()
                .map(client_rect_from_quad)
                .reduce(union_client_rect)
                .unwrap_or_else(zero_client_rect)),
            _ => Err(provider_contract_error("bounding client rects")),
        })
        .collect()
}

pub(crate) fn observable_box_model(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<Option<LayoutBoxModel>, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(None);
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(None);
    };
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::BoxModel { source }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::BoxModel(model)) => Ok(model),
        _ => Err(provider_contract_error("box model")),
    }
}

/// Resolve one viewport-relative box for scroll anchoring. Real layout already
/// projects root scrolling into viewport coordinates; the explicit adjustment
/// is retained only inside the legacy Mock provider.
pub(crate) fn observable_scroll_adjusted_client_rect(
    runtime: &JsContextHost,
    source: DomHandle,
    scroll_x: f64,
    scroll_y: f64,
    reason: LayoutFlushReason,
) -> Result<ClientRect, LayoutError> {
    if runtime.layout_policy().uses_real_layout() {
        observable_bounding_client_rect(runtime, source, reason)
    } else {
        Ok(compute_mock_scroll_adjusted_client_rect(
            runtime, source, scroll_x, scroll_y,
        ))
    }
}

pub(crate) fn observable_event_offset(
    runtime: &JsContextHost,
    source: DomHandle,
    point: LayoutPoint,
    reason: LayoutFlushReason,
) -> Result<LayoutPoint, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(point);
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(point);
    };
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::EventOffset { source, point }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::EventOffset(offset)) => Ok(offset.unwrap_or(point)),
        _ => Err(provider_contract_error("event offset")),
    }
}

pub(crate) fn observable_element_metrics(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<Option<LayoutElementMetrics<DomHandle>>, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(None);
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(None);
    };
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::ElementMetrics { source }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::ElementMetrics(metrics)) => Ok(metrics),
        _ => Err(provider_contract_error("element metrics")),
    }
}

pub(crate) fn observable_used_grid_tracks(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<Option<LayoutResolvedGridTracks>, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(None);
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(None);
    };
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::UsedGridTracks { source }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::UsedGridTracks(tracks)) => Ok(tracks),
        _ => Err(provider_contract_error("used Grid tracks")),
    }
}

fn dom_child_elements(dom: &moli_dom::native::NativeDom, parent: DomHandle) -> Vec<DomHandle> {
    let mut children = Vec::new();
    let mut child = dom.first_child(parent);
    while let Some(c) = child {
        if dom.node(c).is_some_and(|node| node.is_element()) {
            children.push(c);
        }
        child = dom.next_sibling(c);
    }
    children
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn is_sentinel_candidate(runtime: &JsContextHost, source: DomHandle) -> bool {
    const TOKENS: &[&str] = &[
        "sentinel",
        "loadmore",
        "load-more",
        "load_more",
        "infinitescroll",
        "infinite-scroll",
        "infinite_scroll",
        "bottomobserver",
        "bottom-observer",
        "bottom_observer",
        "loadinganchor",
        "loading-anchor",
        "loading_anchor",
        "scrollanchor",
        "scroll-anchor",
        "scroll_anchor",
    ];

    let test_token = |s: &str| {
        TOKENS
            .iter()
            .any(|&token| contains_ignore_ascii_case(s, token))
    };
    let dom = runtime.dom_host().dom();
    dom.node(source)
        .and_then(|node| node.as_element())
        .is_some_and(|el| {
            el.attribute("class").is_some_and(test_token)
                || el.attribute("id").is_some_and(test_token)
                || el.attribute("data-sentinel").is_some()
                || el.attribute("data-testid").is_some_and(test_token)
                || el.attribute("role").is_some_and(|r| {
                    r.eq_ignore_ascii_case("status") || r.eq_ignore_ascii_case("progressbar")
                })
                || el.attribute("aria-live").is_some()
        })
}

fn derive_container_extrapolated_geometry(
    runtime: &JsContextHost,
    tree: &moli_layout::FrozenLayoutTree<DomHandle>,
    dom_elements: &[DomHandle],
    target_idx: usize,
    is_sentinel: bool,
) -> Option<(LayoutRect, Vec<LayoutScrollContainerMetrics<DomHandle>>)> {
    if dom_elements.len() < 2 {
        return None;
    }

    // Tiered Layout Fidelity & Conservative Envelope (方案三):
    // Collect all existing layout boxes in the container to provide Tier 0 exact hit
    // and extract grid layout parameters for Tier 1 extrapolation.
    // Exclude sentinel candidates to prevent 10px sentinels from polluting the card baseline.
    let mut baseline = Vec::new();
    for (idx, &child) in dom_elements.iter().enumerate() {
        if is_sentinel_candidate(runtime, child) {
            continue;
        }
        let quads = tree.client_rects_for_source(child);
        if let Some(rect) = quads.first().map(|q| q.bounding_rect())
            && rect.width > 0.0
            && rect.height > 0.0
        {
            baseline.push((idx, rect));
        }
    }
    if baseline.len() < 2 {
        return None;
    }

    let baseline_child = dom_elements[baseline[0].0];
    let mut parent_containers = tree
        .scroll_into_view_geometry_for_source(baseline_child)
        .map(|geom| geom.scroll_containers)?;

    let mut shift_x = 0.0f32;
    let mut shift_y = 0.0f32;
    let mut tree_scroll_y = 0.0f64;
    for (idx, scroll_container) in parent_containers.iter_mut().enumerate() {
        let (curr_scroll_x, curr_scroll_y) = node_scroll_position(runtime, scroll_container.source);
        let t_sx = f64::from(scroll_container.metrics.scroll_offset.x);
        let t_sy = f64::from(scroll_container.metrics.scroll_offset.y);
        if idx == 0 {
            tree_scroll_y = t_sy;
        }
        shift_x += (curr_scroll_x - t_sx) as f32;
        shift_y += (curr_scroll_y - t_sy) as f32;
        scroll_container.metrics.scroll_offset.x = curr_scroll_x as f32;
        scroll_container.metrics.scroll_offset.y = curr_scroll_y as f32;
    }

    let x0 = baseline[0].1.x;
    let y0 = baseline[0].1.y;
    let card_w = baseline[0].1.width;
    let card_h = baseline[0].1.height;

    // Detect column count: items on the first row (y within 5px of y0)
    let cols = baseline
        .iter()
        .take_while(|(_, r)| (r.y - y0).abs() < 5.0)
        .count()
        .max(1);

    let h0 = baseline[0].1.height;
    let is_uniform = baseline.iter().all(|(_, r)| {
        let diff = (r.height - h0).abs();
        diff < 50.0 || (h0 > 0.0 && diff / h0 < 0.35)
    });
    if !is_uniform {
        return None;
    }

    let delta_x = if cols > 1 && baseline.len() > 1 {
        (baseline[1].1.x - x0).abs().max(card_w)
    } else {
        card_w
    };

    let delta_y = baseline
        .iter()
        .find(|(_, r)| (r.y - y0) >= card_h * 0.5)
        .map(|(_, r)| (r.y - y0).max(card_h))
        .unwrap_or(card_h);

    let (target_x, target_y, target_w, target_h) = if is_sentinel {
        // Problem 1: target_idx represents the exact number of cards preceding this sentinel.
        let card_count = target_idx;
        let total_rows = card_count.div_ceil(cols) as f32;
        let tx = x0 - shift_x;
        let ty = y0 + total_rows * delta_y - shift_y;
        (tx, ty, card_w, 10.0f32)
    } else if let Some((_, exact_rect)) = baseline.iter().find(|(idx, _)| *idx == target_idx) {
        // Tier 0: Exact layout hit for already-rendered elements
        (
            exact_rect.x - shift_x,
            exact_rect.y - shift_y,
            exact_rect.width,
            exact_rect.height,
        )
    } else {
        // Tier 1: Conservative Envelope extrapolation for appended elements
        let row = (target_idx / cols) as f32;
        let col = (target_idx % cols) as f32;
        let tx = x0 + col * delta_x - shift_x;
        let ty = y0 + row * delta_y - shift_y;
        (tx, ty, card_w, card_h)
    };

    if let Some(scroll_container) = parent_containers.first_mut() {
        // Problem 2 & 5: Content bottom distinguishes sentinel (10.0px) from real cards (card_h),
        // and uses target_idx for sentinel to avoid 1-row offset when sentinel is inside container.
        let card_rows = if is_sentinel {
            target_idx.div_ceil(cols) as f32
        } else {
            dom_elements.len().div_ceil(cols) as f32
        };
        let content_bottom_offset = if is_sentinel {
            card_rows * delta_y + 10.0
        } else {
            (card_rows - 1.0).max(0.0) * delta_y + card_h
        };
        let content_bottom = (y0 - scroll_container.metrics.scrollport.points[0].y
            + (tree_scroll_y as f32))
            + content_bottom_offset;
        let new_max_y = (content_bottom - scroll_container.metrics.client_size.height).max(0.0);
        scroll_container.metrics.maximum_scroll_offset.y = scroll_container
            .metrics
            .maximum_scroll_offset
            .y
            .max(new_max_y);
    }

    Some((
        LayoutRect::new(target_x, target_y, target_w, target_h),
        parent_containers,
    ))
}

pub(crate) fn try_append_only_target_geometry(
    runtime: &JsContextHost,
    document: DomHandle,
    source: DomHandle,
) -> Option<(LayoutRect, Vec<LayoutScrollContainerMetrics<DomHandle>>)> {
    runtime
        .with_latest_layout_tree_for_document(document, |tree| {
            let dom = runtime.dom_host().dom();

            // Case A: source is a direct child inside a container (e.g. card in grid or tail sentinel)
            // Problem 2: Strictly limited to direct parent to avoid false positives on distant ancestors,
            // and evaluated before Case B so internal sentinels are not misidentified as following the last card.
            if let Some(parent) = dom.parent_node(source) {
                let dom_elements = dom_child_elements(dom, parent);
                if dom_elements.len() >= 2
                    && let Some(source_idx) = dom_elements.iter().position(|&c| c == source)
                {
                    // Allow sentinel to be within the last 2 items (tolerates a trailing loading spinner)
                    let is_tail = source_idx >= dom_elements.len().saturating_sub(2);
                    let is_sentinel = is_tail && is_sentinel_candidate(runtime, source);

                    if let Some(res) = derive_container_extrapolated_geometry(
                        runtime,
                        tree,
                        &dom_elements,
                        source_idx,
                        is_sentinel,
                    ) {
                        return Some(res);
                    }
                }
            }

            // Case B: source is an immediate sibling sentinel following a container
            // Problem 2: Require is_sentinel_candidate and limit search to up to 4 siblings
            if is_sentinel_candidate(runtime, source) {
                let mut prev = dom.previous_sibling(source);
                let mut scan_count = 0;
                while let Some(sibling) = prev
                    && scan_count < 4
                {
                    if dom.node(sibling).is_some_and(|n| n.is_element()) {
                        scan_count += 1;
                        let sibling_children = dom_child_elements(dom, sibling);
                        if sibling_children.len() >= 2
                            && let Some(res) = derive_container_extrapolated_geometry(
                                runtime,
                                tree,
                                &sibling_children,
                                sibling_children.len(),
                                true,
                            )
                        {
                            return Some(res);
                        }
                    }
                    prev = dom.previous_sibling(sibling);
                }
            }

            // Case D: source is inside an immediate wrapper (depth 1) following a container
            // Problem 2: Strictly limit to 1 ancestor wrapper and require sentinel identity
            if let Some(parent) = dom.parent_node(source)
                && (is_sentinel_candidate(runtime, source)
                    || is_sentinel_candidate(runtime, parent))
            {
                let mut parent_prev = dom.previous_sibling(parent);
                let mut scan_count = 0;
                while let Some(sibling) = parent_prev
                    && scan_count < 4
                {
                    if dom.node(sibling).is_some_and(|n| n.is_element()) {
                        scan_count += 1;
                        let sibling_children = dom_child_elements(dom, sibling);
                        if sibling_children.len() >= 2
                            && let Some(res) = derive_container_extrapolated_geometry(
                                runtime,
                                tree,
                                &sibling_children,
                                sibling_children.len(),
                                true,
                            )
                        {
                            return Some(res);
                        }
                    }
                    parent_prev = dom.previous_sibling(sibling);
                }
            }

            None
        })
        .flatten()
}

fn try_append_only_scroll_into_view_geometry(
    runtime: &JsContextHost,
    document: DomHandle,
    source: DomHandle,
) -> Option<LayoutScrollIntoViewGeometry<DomHandle>> {
    let (target_rect, parent_containers) =
        try_append_only_target_geometry(runtime, document, source)?;
    let target_quad = LayoutTransform2D::IDENTITY.map_rect(target_rect);
    Some(LayoutScrollIntoViewGeometry {
        target_rects: vec![target_quad],
        scroll_containers: parent_containers,
    })
}

pub(crate) fn observable_scroll_into_view_geometry(
    runtime: &JsContextHost,
    source: DomHandle,
    reason: LayoutFlushReason,
) -> Result<Option<LayoutScrollIntoViewGeometry<DomHandle>>, LayoutError> {
    if !runtime.dom_host().is_connected(source) {
        return Ok(None);
    }
    let Some(document) = runtime.layout_document_for_source(source) else {
        return Ok(None);
    };
    if runtime.layout_policy().uses_real_layout() && !runtime.is_latest_layout_dom_fresh() {
        if let Some(fast_geom) =
            try_append_only_scroll_into_view_geometry(runtime, document, source)
        {
            return Ok(Some(fast_geom));
        }
        runtime.clear_latest_layout();
    }
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![LayoutQuery::ScrollIntoViewGeometry { source }]),
    )?;
    match answers.answers.into_iter().next() {
        Some(LayoutQueryAnswer::ScrollIntoViewGeometry(Some(geometry))) => Ok(Some(geometry)),
        Some(LayoutQueryAnswer::ScrollIntoViewGeometry(None)) => {
            if runtime.layout_policy().uses_real_layout() {
                if let Some(fast_geom) =
                    try_append_only_scroll_into_view_geometry(runtime, document, source)
                {
                    return Ok(Some(fast_geom));
                }
                runtime.clear_latest_layout();
                let fresh_answers = runtime.answer_layout_for_document(
                    document,
                    reason,
                    &LayoutQueryBatch::new(vec![LayoutQuery::ScrollIntoViewGeometry { source }]),
                )?;
                match fresh_answers.answers.into_iter().next() {
                    Some(LayoutQueryAnswer::ScrollIntoViewGeometry(geom)) => Ok(geom),
                    _ => Err(provider_contract_error("scroll-into-view geometry")),
                }
            } else {
                Ok(None)
            }
        }
        _ => Err(provider_contract_error("scroll-into-view geometry")),
    }
}

pub(crate) fn observable_caret_position(
    runtime: &JsContextHost,
    document: DomHandle,
    point: LayoutPoint,
    reason: LayoutFlushReason,
) -> Result<Option<LayoutCaretPosition<DomHandle>>, LayoutError> {
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![
            LayoutQuery::DocumentMetrics,
            LayoutQuery::CaretPosition { point },
        ]),
    )?;
    let mut answers = answers.answers.into_iter();
    match (answers.next(), answers.next()) {
        (
            Some(LayoutQueryAnswer::DocumentMetrics(metrics)),
            Some(LayoutQueryAnswer::CaretPosition(position)),
        ) => {
            let inside_viewport = point.x >= 0.0
                && point.y >= 0.0
                && point.x < metrics.viewport.css_width as f32
                && point.y < metrics.viewport.css_height as f32;
            Ok(inside_viewport.then_some(position).flatten())
        }
        _ => Err(provider_contract_error("caret position")),
    }
}

pub(crate) fn observable_hit_test_all(
    runtime: &JsContextHost,
    document: DomHandle,
    point: LayoutPoint,
    ignore_pointer_events_none: bool,
    reason: LayoutFlushReason,
) -> Result<(LayoutDocumentMetrics, Vec<LayoutHit<DomHandle>>), LayoutError> {
    let answers = observable_geometry_batch(
        runtime,
        document,
        reason,
        &LayoutQueryBatch::new(vec![
            LayoutQuery::DocumentMetrics,
            LayoutQuery::HitTestAll {
                point,
                ignore_pointer_events_none,
            },
        ]),
    )?;
    let mut answers = answers.answers.into_iter();
    match (answers.next(), answers.next()) {
        (
            Some(LayoutQueryAnswer::DocumentMetrics(metrics)),
            Some(LayoutQueryAnswer::HitTestAll(hits)),
        ) => Ok((metrics, hits)),
        _ => Err(provider_contract_error("complete hit test")),
    }
}

pub(super) fn provider_contract_error(answer: &str) -> LayoutError {
    LayoutError::source_contract(
        "renderer geometry provider",
        format!("returned a mismatched {answer} answer"),
    )
}
