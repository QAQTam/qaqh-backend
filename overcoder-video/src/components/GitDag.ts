export interface DagNode {
  id: string;
  x: number;
  y: number;
  sha: string;
  title: string;
  sub: string;
  kind: 'root' | 'airgap' | 'rebuilt' | 'graft' | 'head';
}

export const DAG_NODES: DagNode[] = [
  { id: 'dsx', x: 110, y: 80, sha: 'b5e92c61', title: '05-14 DSX 根', sub: '80 files · v4.0-α1', kind: 'root' },
  { id: 'airB', x: 265, y: 80, sha: '883ca4f4', title: '空气B · 空树', sub: '改名失落窗口', kind: 'airgap' },
  { id: 'deepx06', x: 420, y: 80, sha: '34e4587f', title: '06-02 DeepX', sub: 'session.json 重建', kind: 'rebuilt' },
  { id: 'airC', x: 575, y: 80, sha: 'df787f55', title: '空气C · 黑暗71天', sub: '空树 4b825dc6', kind: 'airgap' },
  { id: 'deepx07', x: 165, y: 210, sha: 'd3adddbb', title: '07-26 deepx', sub: '298 提交 (ADR-0013)', kind: 'graft' },
  { id: 'qaq0822', x: 375, y: 210, sha: 'd5ba44c7', title: '08-22 赌气重开', sub: '0.0.0-alpha0', kind: 'graft' },
  { id: 'v4', x: 585, y: 210, sha: 'e476aad6', title: '11-14 v4.0.0', sub: '1,426 主干连通', kind: 'head' },
];

export function renderGitDagSvg(container: SVGSVGElement, progress: number, pulse: number): void {
  const w = 700;
  const h = 290;
  container.setAttribute('viewBox', `0 0 ${w} ${h}`);

  // Build path connecting nodes in sequence
  const edges: Array<[DagNode, DagNode, boolean]> = [
    [DAG_NODES[0], DAG_NODES[1], true],
    [DAG_NODES[1], DAG_NODES[2], true],
    [DAG_NODES[2], DAG_NODES[3], true],
    [DAG_NODES[3], DAG_NODES[4], false],
    [DAG_NODES[4], DAG_NODES[5], false],
    [DAG_NODES[5], DAG_NODES[6], false],
  ];

  const revealedEdges = Math.max(0, Math.min(edges.length, progress * edges.length));

  const edgeSvg = edges
    .map(([a, b, isTop], idx) => {
      const local = Math.max(0, Math.min(1, revealedEdges - idx));
      if (local <= 0) return '';
      const d =
        isTop || a.y === b.y
          ? `M ${a.x} ${a.y} L ${b.x} ${b.y}`
          : `M ${a.x} ${a.y} C ${a.x} ${a.y + 70}, ${b.x} ${b.y - 70}, ${b.x} ${b.y}`;
      const isAir = a.kind === 'airgap' || b.kind === 'airgap';
      const totalLen = 460;
      const offset = totalLen * (1 - local);
      return `
        <path d="${d}"
          fill="none"
          stroke="${isAir ? '#22d3ee' : '#fbbf24'}"
          stroke-width="3"
          stroke-dasharray="${isAir ? '8 6' : `${totalLen}`}"
          stroke-dashoffset="${isAir ? 0 : offset}"
          opacity="${local}"
        />
      `;
    })
    .join('');

  const nodeSvg = DAG_NODES.map((n, idx) => {
    const threshold = idx / DAG_NODES.length;
    const appear = Math.max(0, Math.min(1, (progress - threshold * 0.85) / 0.16));
    if (appear <= 0) return '';

    const palette: Record<DagNode['kind'], { fill: string; stroke: string; text: string }> = {
      root: { fill: '#064e3b', stroke: '#10b981', text: '#6ee7b7' },
      airgap: { fill: '#081221', stroke: '#22d3ee', text: '#67e8f9' },
      rebuilt: { fill: '#1e1b4b', stroke: '#818cf8', text: '#c7d2fe' },
      graft: { fill: '#3f1d28', stroke: '#fb7185', text: '#fda4af' },
      head: { fill: '#451a03', stroke: '#fbbf24', text: '#fde68a' },
    };
    const p = palette[n.kind];
    const dash = n.kind === 'airgap' ? 'stroke-dasharray="5 4"' : '';
    const r = n.kind === 'head' ? 24 + Math.sin(pulse * Math.PI * 2) * 1.5 : 21;

    return `
      <g transform="translate(${n.x}, ${n.y}) scale(${0.7 + 0.3 * appear})" opacity="${appear}">
        <circle r="${r + 7}" fill="${p.stroke}" opacity="0.16" />
        <circle r="${r}" fill="${p.fill}" stroke="${p.stroke}" stroke-width="3" ${dash} />
        <text y="4" text-anchor="middle" fill="#f8fafc" font-size="10" font-weight="700" font-family="Cascadia Code, Consolas, monospace">${n.sha.slice(0, 6)}</text>
        <text y="-28" text-anchor="middle" fill="${p.text}" font-size="13" font-weight="700" font-family="Microsoft YaHei UI, sans-serif">${n.title}</text>
        <rect x="-58" y="25" width="116" height="19" rx="5" fill="rgba(5, 7, 13, 0.88)" />
        <text y="38" text-anchor="middle" fill="#cbd5e1" font-size="11" font-family="Microsoft YaHei UI, sans-serif">${n.sub}</text>
      </g>
    `;
  }).join('');

  container.innerHTML = `${edgeSvg}${nodeSvg}`;
}
