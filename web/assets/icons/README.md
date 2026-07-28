# Transit map icon pack

Clean 24×24 SVG icons for the transit web UI. Most icons use `stroke="currentColor"` / `fill="currentColor"` so they inherit text color from CSS. Map pins use fixed green/red fills for origin/destination recognition.

**Path (from web app root):** `assets/icons/<file>.svg`

**Machine-readable index:** [`manifest.json`](./manifest.json)

## Files

| File | ID | Intended use |
|------|-----|----------------|
| `train.svg` | `train` | Rail / TGV-style intercity train mode |
| `bus.svg` | `bus` | Bus and coach |
| `metro.svg` | `metro` | Metro / subway |
| `tram.svg` | `tram` | Tram / light rail |
| `ferry.svg` | `ferry` | Ferry / water transit |
| `walk.svg` | `walk` | Walking / pedestrian leg |
| `wheelchair.svg` | `wheelchair` | Accessibility badge |
| `bike.svg` | `bike` | Bicycle / bike-share |
| `transfer.svg` | `transfer` | Connection / interchange arrows |
| `pin-origin.svg` | `pin-origin` | Green start marker on the map |
| `pin-destination.svg` | `pin-destination` | Red end marker on the map |
| `alert.svg` | `alert` | Warning triangle (service alerts) |
| `delay.svg` | `delay` | Delay / late clock |
| `live.svg` | `live` | Realtime / live data pulse |
| `station.svg` | `station` | Station / gare building |
| `platform.svg` | `platform` | Platform / track sign |
| `vehicle.svg` | `vehicle` | Vehicle position (circle + heading) |
| `sprite.svg` | — | Optional single-file symbol sprite |
| `icons.css` | — | Optional CSS utility classes |

## Usage

### Inline or `<img>`

```html
<img src="assets/icons/bus.svg" alt="" width="24" height="24" />
```

### CSS `currentColor` (inline SVG or mask)

```html
<span class="icon icon-bus" style="color: #2563eb"></span>
```

```css
.icon {
  display: inline-block;
  width: 1.25rem;
  height: 1.25rem;
  background-color: currentColor;
  -webkit-mask: var(--icon) center / contain no-repeat;
  mask: var(--icon) center / contain no-repeat;
}
.icon-bus { --icon: url("./bus.svg"); }
```

See `icons.css` for a full class list.

### Vehicle heading

`vehicle.svg` points “up” (north). Rotate the element to match bearing:

```html
<img class="vehicle-marker" src="assets/icons/vehicle.svg"
     style="transform: rotate(120deg)" alt="" />
```

### Pins

`pin-origin.svg` and `pin-destination.svg` keep brand colors (`#22c55e` / `#ef4444`). Do not recolor with `currentColor` unless you replace fills intentionally.

### Sprite

```html
<svg width="24" height="24" aria-hidden="true">
  <use href="assets/icons/sprite.svg#icon-metro" />
</svg>
```

## Design notes

- ViewBox: `0 0 24 24`
- Stroke icons: ~1.75px stroke, round caps/joins
- Prefer monochrome + CSS color over multicolor (except pins)
- Suitable for map overlays, itinerary steps, filters, and status chips
