
pub trait Rect {
    /// Returns width.
    fn width(&self) -> f64;

    /// Returns height.
    fn height(&self) -> f64;
}

impl Rect for (f64, f64) {
    #[inline]
    fn width(&self) -> f64 { self.0 }
    #[inline]
    fn height(&self) -> f64 { self.1 }
}

#[derive(Debug, Copy, Clone)]
pub struct Box {
    pub id: usize,
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Copy, Clone)]
struct Space {
    w: f64,
    h: f64,
    x: f64,
    y: f64,
}

#[derive(Debug)]
pub struct Layout {
    pub width: f64,
    pub height: f64,
    pub fill_ratio: f64,
    pub items: Vec<Box>,
}

impl Layout {
    pub fn new<I: IntoIterator<Item=impl Rect>>(items: I) -> Self {
        let boxes: Vec<_> = items.into_iter().enumerate()
            .map(|(idx, item)| Box {
                id: idx,
                w: item.width(),
                h: item.height(),
                x: f64::NAN,
                y: f64::NAN,
            } ).collect();
        Self::from_boxes(boxes)
    }

    fn from_boxes(mut boxes: Vec<Box>) -> Self {
        let total_area: f64 = boxes.iter().map(|b| b.h * b.w).sum();
        let max_width = boxes.iter().map(|b| b.w)
            .fold(f64::NEG_INFINITY, f64::max);
        // sort the boxes for insertion by height, descending
        boxes.sort_unstable_by(|a, b| b.h.partial_cmp(&a.h).unwrap());
        // aim for a squarish resulting container,
        // slightly adjusted for sub-100% space utilization
        let start_width = (total_area / 0.95).sqrt().ceil().max(max_width);

        // start with a single empty space, unbounded at the bottom
        let mut spaces = vec![
            Space { x: 0., y: 0., w: start_width, h: f64::MAX }
        ];

        let mut width: f64 = 0.;
        let mut height: f64 = 0.;

        for mut b in boxes.iter_mut() {
            // look through spaces backwards so that we check smaller spaces first
            for (space_idx, space) in spaces.iter_mut().enumerate().rev() {
                // look for empty spaces that can accommodate the current box
                if b.w > space.w || b.h > space.h { continue; }

                // found the space; add the box to its top-left corner
                // |-------|-------|
                // |  box  |       |
                // |_______|       |
                // |         space |
                // |_______________|
                b.x = space.x;
                b.y = space.y;

                height = height.max(b.y + b.h);
                width = width.max(b.x + b.w);

                if b.w == space.w && b.h == space.h {
                    // space matches the box exactly; remove it
                    spaces.swap_remove(space_idx);
                } else if b.h == space.h {
                    // space matches the box height; update it accordingly
                    // |-------|---------------|
                    // |  box  | updated space |
                    // |_______|_______________|
                    space.x += b.w;
                    space.w -= b.w;
                } else if b.w == space.w {
                    // space matches the box width; update it accordingly
                    // |---------------|
                    // |      box      |
                    // |_______________|
                    // | updated space |
                    // |_______________|
                    space.y += b.h;
                    space.h -= b.h;
                } else {
                    // otherwise the box splits the space into two spaces
                    // |-------|-----------|
                    // |  box  | new space |
                    // |_______|___________|
                    // | updated space     |
                    // |___________________|
                    let new_space = Space {
                        x: space.x + b.w,
                        y: space.y,
                        w: space.w - b.w,
                        h: b.h
                    };
                    space.y += b.h;
                    space.h -= b.h;
                    spaces.push(new_space);
                }
                break;
            }
        }

        let fill_ratio = if width != 0. && height != 0. {
                total_area / (width * height)
            } else { 1. };

        Self {
            width,
            height,
            fill_ratio,
            items: boxes,
        }
    }
}