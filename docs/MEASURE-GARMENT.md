# Garment measurements from one photo (`agenthand measure --garment`)

agenthand's scale-card estimator can read the flat-lay measurements tailors,
seamstresses and resellers use for top-wear (jackets, shirts, hoodies) from a
single photo: the garment laid flat, a printed agenthand card on the same
surface, and the product mask from `cut_out`.

```
agenthand measure <photo> <card-id> --mask <mask.png> --garment \
    [--mask-edge-px <px>] [--diagram <out.png>] [--diagram-note <text>] \
    [--profile <layout-profile.json>]
```

- `<card-id>` is the id printed on the card in the photo (for example `AH-57`).
- `--mask` is an 8-bit mask, product = 255, the same size and orientation as
  the photo. Pixels at 128 or above count as product. With `--mask` the
  estimator's own extent also comes from the product pixels.
- `--mask-edge-px` is how sharp the mask's edge is, in photo pixels (default
  2). A `cut_out` mask is upscaled from the model's working size, so it is
  only as sharp as one working pixel: photo width / working width (for a
  4284 px photo and a 1206 px working mask, 3.552).
- `--diagram` writes a 1920 x 1080 PNG: the outline seen from above, collar
  up, with the landmarks, measurement lines and labels in cm, drawn in code
  (no model calls, no font files).
- `--diagram-note` adds a red note line to the diagram (for example to mark a
  provisional mask).

The output is the estimator's JSON with one more key, `garment`. Every
existing field is unchanged.

## The stated assumption

The garment lies flat on the same surface as the card. Its outline (the mask)
is mapped into the card plane through the card homography. Parts raised off
the surface (a thick collar, a rolled cuff) are measured as their outline seen
from above. Every size also assumes the card was printed at 100%, as the
estimator states in `card_print_check`.

## What is measured, and how

All figures are in the garment's own frame: the centre axis runs from the
collar to the hem. "Left" and "right" are as drawn on the diagram (collar up),
not the wearer's.

| Key | Measurement | Landmarks and method |
|---|---|---|
| `body_length` | Body length | High point of shoulder (where the collar or neckline meets the shoulder line in the outline) straight down to the hem, parallel to the centre axis; mean of both sides |
| `chest_width` | Chest, pit to pit | Straight across the body 1 cm below the lower of the two underarm points (the concave corners where each sleeve meets the side), side edge to side edge |
| `shoulder_width` | Shoulder width | Straight line between the shoulder corners of the outline (where the shoulder line turns into the sleeve's top edge) |
| `sleeve_length`, `sleeve_length_left`, `sleeve_length_right` | Sleeve length | Along the sleeve's top edge from the shoulder corner to the outer cuff corner; the headline is the mean of the measurable sides |
| `underarm_sleeve_left`, `underarm_sleeve_right` | Underarm sleeve length | Along the sleeve's underside (the underarm seam) from the underarm point to the inner cuff corner. It needs no shoulder seam, so it holds on dropped shoulders |
| `hem_width` | Hem width | Straight across the bottom of the body 1 cm above the highest point of the hem edge (middle 80% of its width), side edge to side edge |

Across-back is not attempted: on a front-up flat lay the back faces the floor.

### Finding the landmarks

1. The mask's outer outline is traced at photo resolution, mapped into the
   card plane (mm), smoothed (Gaussian, sigma 2 mm) and resampled every 1 mm.
2. The torso core is the outline opened with a disk of 0.6 times its widest
   inscribed radius. What the opening removes and reaches out from the torso
   are the sleeves (a collar or hood is short and wide, so it does not count).
3. The centre axis is the mirror axis of the lower torso (below the sleeve
   roots, where a folded or raised sleeve does not reach). The hem is the end
   the torso reaches farther from the sleeve roots.
4. Corners are read from the turning angle of the outline at several chord
   scales: shoulder corner (convex, at least 12 degrees), underarm (concave,
   at least 25), high point of shoulder (the first corner of at least 25
   degrees walking from the shoulder to the collar), cuff corners (convex, at
   least 35).

### Uncertainty

Each figure carries `plus_minus_cm`, a half range:

`card_fit + sqrt(mask_edge^2 + landmark^2 + contour_step^2)`

- `card_fit`: the estimator's own range model at the figure's midpoint,
  2u + 1% + small-card term, u = (fit RMS / px per mm) x (1 + lever)^2.
  It grows with the distance from the card.
- `mask_edge`: `--mask-edge-px` converted to mm at each endpoint, added in
  quadrature.
- `landmark`: how far the figure moves across the convention band (the chest
  line 0.5 to 2.5 cm below the underarm, the hem line 0.5 to 2.5 cm above the
  hem, the direct underarm-to-underarm distance), across corner scales,
  between the two sides, and, for a sleeve bent more than 10 degrees, the
  difference between its edge and its centre line.
- `contour_step`: half the 1 mm resample step at each end.

The components are in `uncertainty_mm` so a reader can see what dominates.

### Not measurable

A figure whose landmark the pose hides is reported with `"measurable": false`,
`"value_cm": null` and the reason, for example:

- a sleeve folded over or under the body: that side's sleeve, the chest and the
  shoulder width;
- a shoulder line that runs into the sleeve without a corner (dropped or raglan
  shoulders): shoulder width and shoulder-to-cuff sleeve length;
- a collar or hood that is flipped or merges into the shoulder line: body
  length;
- a sleeve lying against the body below the underarm: chest.

No figure is filled in by estimate when its landmark is hidden.

## Tests

`tests/measure_garment.rs` renders a synthetic T-shaped jacket of known size
in the plane of the printed AH-BIG card through a known projective homography
(about 1.3 px/mm), finds the card with the real detector, and reads every
figure back from the mask. Each figure must be within 4 mm of the same
landmarks carried through the fitted card plane (pixel quantization and the
resample step at both ends), and the physical size must lie inside the stated
range. A second case folds one sleeve across the body: that sleeve, the chest
and the shoulder width report not measurable with a reason, and the rest still
measures.
