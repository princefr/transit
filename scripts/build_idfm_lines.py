#!/usr/bin/env python3
"""Merge référentiel des lignes + IDFM traces into web/assets/idfm/lignes.geojson."""
import json, sys
from pathlib import Path
from collections import Counter

def douglas_peucker(points, epsilon):
    if len(points) < 3:
        return points
    def dist_point_line(p, a, b):
        ax, ay = a; bx, by = b; px, py = p
        dx, dy = bx - ax, by - ay
        if dx == 0 and dy == 0:
            return ((px - ax)**2 + (py - ay)**2) ** 0.5
        t = max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx*dx + dy*dy)))
        qx, qy = ax + t * dx, ay + t * dy
        return ((px - qx)**2 + (py - qy)**2) ** 0.5
    dmax, idx = 0.0, 0
    for i in range(1, len(points) - 1):
        d = dist_point_line(points[i], points[0], points[-1])
        if d > dmax:
            idx, dmax = i, d
    if dmax > epsilon:
        left = douglas_peucker(points[: idx + 1], epsilon)
        right = douglas_peucker(points[idx:], epsilon)
        return left[:-1] + right
    return [points[0], points[-1]]

def simplify_geom(geom, epsilon):
    if not geom: return None
    t = geom.get('type')
    if t == 'LineString':
        coords = geom['coordinates']
        if len(coords) < 2: return geom
        simp = douglas_peucker([(c[0], c[1]) for c in coords], epsilon)
        return {'type': 'LineString', 'coordinates': [[x, y] for x, y in simp]}
    if t == 'MultiLineString':
        parts = []
        for part in geom['coordinates']:
            if not part or len(part) < 2: continue
            simp = douglas_peucker([(c[0], c[1]) for c in part], epsilon)
            if len(simp) >= 2:
                parts.append([[x, y] for x, y in simp])
        if not parts: return None
        if len(parts) == 1:
            return {'type': 'LineString', 'coordinates': parts[0]}
        return {'type': 'MultiLineString', 'coordinates': parts}
    return geom

def main():
    ref_path = Path(sys.argv[1] if len(sys.argv) > 1 else '/home/ondonda/Downloads/referentiel-des-lignes.geojson')
    traces_path = Path(sys.argv[2] if len(sys.argv) > 2 else '/tmp/idfm_traces.geojson')
    root = Path(__file__).resolve().parents[1]
    out = root / 'web' / 'assets' / 'idfm' / 'lignes.geojson'
    out.parent.mkdir(parents=True, exist_ok=True)
    data_out = root / 'data' / 'idfm' / 'lignes.geojson'
    data_out.parent.mkdir(parents=True, exist_ok=True)

    ref = json.loads(ref_path.read_text())
    by_id = {f['properties']['id_line']: f['properties'] for f in ref['features']}
    traces = json.loads(traces_path.read_text())
    MODE_EPS = {'Subway': 5e-5, 'Rail': 6e-5, 'Tram': 7e-5, 'Bus': 1.2e-4, 'Funicular': 5e-5, 'CableWay': 5e-5}
    features = []
    for f in traces['features']:
        g = f.get('geometry')
        if not g: continue
        p = f.get('properties') or {}
        ilico = p.get('id_ilico') or (p.get('route_id') or '').replace('IDFM:', '')
        refp = by_id.get(ilico, {})
        rtype = p.get('route_type') or 'Bus'
        sg = simplify_geom(g, MODE_EPS.get(rtype, 1e-4))
        if not sg: continue
        color = p.get('route_color') or refp.get('colourweb_hexa') or '888888'
        if not str(color).startswith('#'): color = '#' + color
        status = refp.get('status') or 'active'
        mode = (refp.get('transportmode') or rtype or 'bus').lower()
        mode = {'subway':'metro','rail':'rail','tram':'tram','bus':'bus','funicular':'funicular','cableway':'cableway','metro':'metro'}.get(mode, mode)
        features.append({'type':'Feature','geometry':sg,'properties':{
            'id': ilico, 'route_id': p.get('route_id'),
            'short_name': p.get('route_short_name') or refp.get('shortname_line') or '',
            'name': p.get('route_long_name') or refp.get('name_line') or '',
            'mode': mode, 'color': color, 'status': status,
            'network': p.get('networkname') or refp.get('networkname'),
            'operator': p.get('operatorname') or refp.get('operatorname'),
            'soon': status != 'active',
        }})
    fc = {'type':'FeatureCollection','features':features,'meta':{'n':len(features),'source':'IDFM traces+référentiel'}}
    text = json.dumps(fc, ensure_ascii=False, separators=(',',':'))
    out.write_text(text)
    data_out.write_text(text)
    print('wrote', len(features), 'features', out, '%.1fMB' % (out.stat().st_size/1e6))

if __name__ == '__main__':
    main()
