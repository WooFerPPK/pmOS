//! Isolation tests for `toolkit::layout::{Row, Column, row_with_grow, col_with_grow}`.

use toolkit::draw::Rect;
use toolkit::layout::{col_with_grow, row_with_grow, Column, Item, Row};

// ---- Row: placement -----------------------------------------------

#[test]
fn row_places_three_rects_with_correct_x_positions() {
    // Parent (0, 0, 200, 50), padding 5, spacing 10.
    // Interior is (5, 5, 190, 40).
    let mut row = Row::new(Rect::new(0, 0, 200, 50), 5, 10);

    let a = row.next(20, 20);
    let b = row.next(30, 20);
    let c = row.next(25, 20);

    // Child x positions advance by width + spacing.
    assert_eq!(a.x, 5);
    assert_eq!(b.x, 5 + 20 + 10);
    assert_eq!(c.x, 5 + 20 + 10 + 30 + 10);

    // Widths are exactly as requested — none of them overflow the
    // 190-pixel interior.
    assert_eq!(a.width, 20);
    assert_eq!(b.width, 30);
    assert_eq!(c.width, 25);
}

#[test]
fn row_respects_padding_at_left_edge() {
    // Parent starts at (100, 200); padding pushes interior to x=110.
    let mut row = Row::new(Rect::new(100, 200, 200, 50), 10, 0);
    let a = row.next(30, 20);
    assert_eq!(a.x, 110);
}

#[test]
fn row_respects_spacing_between_children() {
    let mut row = Row::new(Rect::new(0, 0, 200, 50), 0, 7);
    let a = row.next(10, 20);
    let b = row.next(10, 20);
    assert_eq!(a.x, 0);
    assert_eq!(b.x, 0 + 10 + 7);
}

#[test]
fn row_centers_children_vertically_in_parent_interior() {
    // Interior height = 40. Children get centred per-call on the
    // cross axis, so different child heights land on different y
    // offsets from the interior top.
    let mut row = Row::new(Rect::new(0, 0, 100, 50), 5, 0);

    let a = row.next(10, 20);
    let b = row.next(10, 30);
    let c = row.next(10, 40);

    // Interior y = 5. (40 - 20)/2 = 10 → y = 15.
    assert_eq!(a.y, 15);
    // (40 - 30)/2 = 5 → y = 10.
    assert_eq!(b.y, 10);
    // (40 - 40)/2 = 0 → y = 5.
    assert_eq!(c.y, 5);
}

#[test]
fn row_returns_clipped_rect_when_child_overflows_remaining_width() {
    // Interior width = 100.
    let mut row = Row::new(Rect::new(0, 0, 100, 50), 0, 0);

    let a = row.next(60, 20);
    assert_eq!(a.width, 60);
    assert_eq!(row.remaining(), 40);

    // Requesting 60 more when only 40 remain → width clipped to 40.
    let b = row.next(60, 20);
    assert_eq!(b.width, 40);
    assert_eq!(b.x, 60);

    // Further requests after overflow return empty-width rects.
    let c = row.next(10, 20);
    assert_eq!(c.width, 0);
    assert!(c.is_empty());
}

#[test]
fn row_remaining_decreases_correctly_after_each_next() {
    // Interior width = 100, spacing = 5.
    let mut row = Row::new(Rect::new(0, 0, 100, 50), 0, 5);
    assert_eq!(row.remaining(), 100);

    row.next(20, 20);
    assert_eq!(row.remaining(), 100 - (20 + 5));

    row.next(10, 20);
    assert_eq!(row.remaining(), 100 - (20 + 5 + 10 + 5));

    // After an overflow, remaining saturates to 0 and stays there.
    row.next(200, 20);
    assert_eq!(row.remaining(), 0);
    row.next(5, 20);
    assert_eq!(row.remaining(), 0);
}

// ---- Column: placement --------------------------------------------

#[test]
fn column_places_three_rects_with_correct_y_positions() {
    // Parent (0, 0, 50, 200), padding 5, spacing 10.
    // Interior is (5, 5, 40, 190).
    let mut col = Column::new(Rect::new(0, 0, 50, 200), 5, 10);

    let a = col.next(20, 20);
    let b = col.next(20, 30);
    let c = col.next(20, 25);

    assert_eq!(a.y, 5);
    assert_eq!(b.y, 5 + 20 + 10);
    assert_eq!(c.y, 5 + 20 + 10 + 30 + 10);

    assert_eq!(a.height, 20);
    assert_eq!(b.height, 30);
    assert_eq!(c.height, 25);
}

#[test]
fn column_respects_padding_at_top_edge() {
    let mut col = Column::new(Rect::new(100, 200, 50, 200), 10, 0);
    let a = col.next(20, 30);
    assert_eq!(a.y, 210);
}

#[test]
fn column_respects_spacing_between_children() {
    let mut col = Column::new(Rect::new(0, 0, 50, 200), 0, 7);
    let a = col.next(10, 10);
    let b = col.next(10, 10);
    assert_eq!(a.y, 0);
    assert_eq!(b.y, 0 + 10 + 7);
}

#[test]
fn column_centers_children_horizontally_in_parent_interior() {
    // Interior width = 40.
    let mut col = Column::new(Rect::new(0, 0, 50, 200), 5, 0);

    let a = col.next(20, 10);
    let b = col.next(30, 10);
    let c = col.next(40, 10);

    // (40 - 20)/2 = 10 → x = 5 + 10 = 15.
    assert_eq!(a.x, 15);
    // (40 - 30)/2 = 5 → x = 10.
    assert_eq!(b.x, 10);
    // (40 - 40)/2 = 0 → x = 5.
    assert_eq!(c.x, 5);
}

#[test]
fn column_returns_clipped_rect_when_child_overflows_remaining_height() {
    let mut col = Column::new(Rect::new(0, 0, 50, 100), 0, 0);

    let a = col.next(20, 60);
    assert_eq!(a.height, 60);
    assert_eq!(col.remaining(), 40);

    let b = col.next(20, 60);
    assert_eq!(b.height, 40);
    assert_eq!(b.y, 60);

    let c = col.next(20, 10);
    assert_eq!(c.height, 0);
    assert!(c.is_empty());
}

// ---- degenerate parents -------------------------------------------

#[test]
fn row_with_zero_width_parent_does_not_panic() {
    let mut row = Row::new(Rect::new(0, 0, 0, 50), 0, 0);
    let a = row.next(10, 20);
    assert!(a.is_empty());
    assert_eq!(row.remaining(), 0);
    // Subsequent calls still don't panic.
    let _ = row.next(5, 5);
}

#[test]
fn column_with_zero_height_parent_does_not_panic() {
    let mut col = Column::new(Rect::new(0, 0, 50, 0), 0, 0);
    let a = col.next(10, 20);
    assert!(a.is_empty());
    assert_eq!(col.remaining(), 0);
    let _ = col.next(5, 5);
}

#[test]
fn row_with_padding_larger_than_parent_returns_empty_rects() {
    // Parent is 20 × 20; padding of 30 saturates the interior to 0.
    let mut row = Row::new(Rect::new(0, 0, 20, 20), 30, 0);
    assert_eq!(row.interior_width(), 0);
    assert_eq!(row.interior_height(), 0);
    assert_eq!(row.remaining(), 0);

    let a = row.next(10, 10);
    assert!(a.is_empty());
    let b = row.next(10, 10);
    assert!(b.is_empty());
}

#[test]
fn column_with_padding_larger_than_parent_returns_empty_rects() {
    let mut col = Column::new(Rect::new(0, 0, 20, 20), 30, 0);
    assert_eq!(col.interior_width(), 0);
    assert_eq!(col.interior_height(), 0);
    assert_eq!(col.remaining(), 0);

    let a = col.next(10, 10);
    assert!(a.is_empty());
}

// ---- sanity / round-trips -----------------------------------------

#[test]
fn row_interior_matches_parent_minus_padding() {
    let row = Row::new(Rect::new(100, 200, 400, 80), 10, 0);
    assert_eq!(row.interior_width(), 400 - 20);
    assert_eq!(row.interior_height(), 80 - 20);
}

#[test]
fn column_interior_matches_parent_minus_padding() {
    let col = Column::new(Rect::new(100, 200, 400, 80), 10, 0);
    assert_eq!(col.interior_width(), 400 - 20);
    assert_eq!(col.interior_height(), 80 - 20);
}

// ---- row_with_grow ------------------------------------------------

#[test]
fn row_grow_single_child_claims_all_leftover() {
    // Interior = 200 wide. One Grow child → gets all 200 px.
    let rects = row_with_grow(
        Rect::new(0, 0, 200, 50),
        0,
        0,
        &[Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].x, 0);
    assert_eq!(rects[0].width, 200);
}

#[test]
fn row_grow_two_equal_weights_split_evenly() {
    // Interior = 200, no padding, no spacing. Two Grow(1) → 100 each.
    let rects = row_with_grow(
        Rect::new(0, 0, 200, 50),
        0,
        0,
        &[Item::Grow(1), Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].width, 100);
    assert_eq!(rects[1].width, 100);
    assert_eq!(rects[0].x, 0);
    assert_eq!(rects[1].x, 100);
}

#[test]
fn row_grow_mixed_fixed_and_grow() {
    // Interior = 300. Fixed(50) uses 50 px; Grow(1) gets remaining 250.
    let rects = row_with_grow(
        Rect::new(0, 0, 300, 50),
        0,
        0,
        &[Item::Fixed(50), Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].width, 50);
    assert_eq!(rects[1].width, 250);
    assert_eq!(rects[1].x, 50);
}

#[test]
fn row_grow_unequal_weights_1_to_2() {
    // Interior = 300. Grow(1) + Grow(2) → 100 + 200.
    let rects = row_with_grow(
        Rect::new(0, 0, 300, 50),
        0,
        0,
        &[Item::Grow(1), Item::Grow(2)],
        20,
    );
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].width, 100);
    assert_eq!(rects[1].width, 200);
}

#[test]
fn row_grow_zero_leftover_produces_zero_width_grow_rects() {
    // Interior = 100; Fixed(100) consumes everything.
    // The Grow child has nothing left → width 0.
    let rects = row_with_grow(
        Rect::new(0, 0, 100, 50),
        0,
        0,
        &[Item::Fixed(100), Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].width, 100);
    assert_eq!(rects[1].width, 0);
}

#[test]
fn row_grow_spacing_deducted_before_grow_split() {
    // Interior = 210, spacing = 10, two Grow(1).
    // Gap between two items = 1 × 10 = 10.
    // Leftover for Grow = 210 - 10 = 200 → each gets 100.
    let rects = row_with_grow(
        Rect::new(0, 0, 210, 50),
        0,
        10,
        &[Item::Grow(1), Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].width, 100);
    assert_eq!(rects[0].x, 0);
    assert_eq!(rects[1].width, 100);
    // x = 0 + 100 (child_w) + 10 (spacing) = 110.
    assert_eq!(rects[1].x, 110);
}

#[test]
fn row_grow_cross_axis_height_and_centring() {
    // Interior height = 50 (parent 60, padding 5). Child height = 30.
    // y_offset = (50-30)/2 = 10. child_y = 5 + 10 = 15.
    let rects = row_with_grow(
        Rect::new(0, 0, 200, 60),
        5,
        0,
        &[Item::Grow(1)],
        30,
    );
    assert_eq!(rects[0].y, 15);
    assert_eq!(rects[0].height, 30);
}

#[test]
fn row_grow_empty_items_returns_empty_vec() {
    let rects = row_with_grow(Rect::new(0, 0, 200, 50), 0, 0, &[], 20);
    assert!(rects.is_empty());
}

#[test]
fn row_grow_grow_weight_zero_treated_as_one() {
    // Two Grow(0) children should share equally (both treated as weight 1).
    let rects = row_with_grow(
        Rect::new(0, 0, 200, 50),
        0,
        0,
        &[Item::Grow(0), Item::Grow(0)],
        20,
    );
    assert_eq!(rects[0].width, 100);
    assert_eq!(rects[1].width, 100);
}

// ---- col_with_grow ------------------------------------------------

#[test]
fn col_grow_single_child_claims_all_leftover() {
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 200),
        0,
        0,
        &[Item::Grow(1)],
        20,
    );
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].y, 0);
    assert_eq!(rects[0].height, 200);
}

#[test]
fn col_grow_two_equal_weights_split_evenly() {
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 200),
        0,
        0,
        &[Item::Grow(1), Item::Grow(1)],
        20,
    );
    assert_eq!(rects[0].height, 100);
    assert_eq!(rects[1].height, 100);
    assert_eq!(rects[0].y, 0);
    assert_eq!(rects[1].y, 100);
}

#[test]
fn col_grow_mixed_fixed_and_grow() {
    // Interior = 300 tall. Fixed(50) + Grow(1) → Grow gets 250.
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 300),
        0,
        0,
        &[Item::Fixed(50), Item::Grow(1)],
        20,
    );
    assert_eq!(rects[0].height, 50);
    assert_eq!(rects[1].height, 250);
    assert_eq!(rects[1].y, 50);
}

#[test]
fn col_grow_unequal_weights_1_to_2() {
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 300),
        0,
        0,
        &[Item::Grow(1), Item::Grow(2)],
        20,
    );
    assert_eq!(rects[0].height, 100);
    assert_eq!(rects[1].height, 200);
}

#[test]
fn col_grow_zero_leftover_produces_zero_height_grow_rects() {
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 100),
        0,
        0,
        &[Item::Fixed(100), Item::Grow(1)],
        20,
    );
    assert_eq!(rects[0].height, 100);
    assert_eq!(rects[1].height, 0);
}

#[test]
fn col_grow_spacing_deducted_before_grow_split() {
    // Interior = 210 tall, spacing = 10, two Grow(1).
    // Leftover = 210 - 10 = 200 → each gets 100.
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 210),
        0,
        10,
        &[Item::Grow(1), Item::Grow(1)],
        20,
    );
    assert_eq!(rects[0].height, 100);
    assert_eq!(rects[0].y, 0);
    assert_eq!(rects[1].height, 100);
    // y = 0 + 100 (child_h) + 10 (spacing) = 110.
    assert_eq!(rects[1].y, 110);
}

#[test]
fn col_grow_cross_axis_width_and_centring() {
    // Interior width = 40 (parent 50, padding 5). Child width = 20.
    // x_offset = (40-20)/2 = 10. child_x = 5 + 10 = 15.
    let rects = col_with_grow(
        Rect::new(0, 0, 50, 200),
        5,
        0,
        &[Item::Grow(1)],
        20,
    );
    assert_eq!(rects[0].x, 15);
    assert_eq!(rects[0].width, 20);
}

#[test]
fn col_grow_empty_items_returns_empty_vec() {
    let rects = col_with_grow(Rect::new(0, 0, 50, 200), 0, 0, &[], 20);
    assert!(rects.is_empty());
}
