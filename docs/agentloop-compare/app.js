(function () {
  const D = window.LOOP_DATA;
  const view = document.getElementById('view');
  const tabBar = document.getElementById('tabs');

  const TABS = [
    { id: 'overview', label: '总览' },
    { id: 'loops', label: '四种 loop' },
    { id: 'matrix', label: '对比矩阵' },
    { id: 'ladder', label: '拆库路线' },
    { id: 'conclusion', label: '结论' }
  ];

  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const num = (n) => n.toLocaleString('en-US');
  const pct = (n) => ((n / D.ladder.total) * 100).toFixed(1) + '%';

  /* ── 顶部 meta ── */
  function renderMeta() {
    document.getElementById('meta').innerHTML = D.meta.sources
      .map((s) => `<span class="pill"><b>${esc(s.name)}</b><code>${esc(s.commit)}</code></span>`)
      .join('');
    document.getElementById('foot-meta').textContent = '核对日期 ' + D.meta.generated;
  }

  /* ── tabs ── */
  function renderTabs(active, onPick) {
    tabBar.innerHTML = TABS.map(
      (t) => `<button role="tab" class="tab ${t.id === active ? 'on' : ''}" data-id="${t.id}">
        ${esc(t.label)}<span class="bar"></span></button>`
    ).join('');
    tabBar.querySelectorAll('.tab').forEach((b) => b.addEventListener('click', () => onPick(b.dataset.id)));
  }

  /* ── flow 图 ── */
  function flowSvg(nodes, color) {
    const W = 172, H = 66, GAP = 46, PAD = 26, TOP = 30;
    const width = PAD * 2 + nodes.length * W + (nodes.length - 1) * GAP;
    const height = 168;
    const kindColor = { in: '#38bdf8', proc: color, out: '#a78bfa', state: '#f59e0b' };
    let s = `<svg class="flow" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" role="img">`;
    s += `<defs><marker id="ar-${color.slice(1)}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="#64748b"/></marker></defs>`;
    nodes.forEach((n, i) => {
      const x = PAD + i * (W + GAP);
      const c = kindColor[n.k] || color;
      s += `<g class="node">
        <rect x="${x}" y="${TOP}" rx="10" width="${W}" height="${H}" fill="#0f172a" stroke="${c}" stroke-opacity=".55"/>
        <rect x="${x}" y="${TOP}" rx="10" width="4" height="${H}" fill="${c}"/>
        <text x="${x + 16}" y="${TOP + 27}" class="nt">${esc(n.t)}</text>
        <text x="${x + 16}" y="${TOP + 47}" class="ns">${esc(n.s)}</text>
      </g>`;
      if (i < nodes.length - 1) {
        const x1 = x + W + 6, x2 = x + W + GAP - 8, y = TOP + H / 2;
        s += `<line x1="${x1}" y1="${y}" x2="${x2}" y2="${y}" stroke="#64748b" stroke-width="1.4" marker-end="url(#ar-${color.slice(1)})"/>`;
      }
    });
    const firstX = PAD + W / 2, lastX = PAD + (nodes.length - 1) * (W + GAP) + W / 2, by = TOP + H;
    s += `<path class="back" d="M ${lastX} ${by} C ${lastX} ${by + 52}, ${firstX} ${by + 52}, ${firstX} ${by + 8}"
      fill="none" stroke="${color}" stroke-opacity=".45" stroke-dasharray="5 5" marker-end="url(#ar-${color.slice(1)})"/>`;
    s += `<text x="${(firstX + lastX) / 2}" y="${by + 46}" class="bl">loop</text>`;
    s += `</svg>`;
    return s;
  }

  /* ── 视图 1：总览 ── */
  function viewOverview() {
    const cards = D.projects.map(
      (p) => `<article class="card" style="--c:${p.color}">
        <div class="card-h">
          <span class="swatch"></span>
          <div>
            <h3>${esc(p.name)}</h3>
            <p class="alias">${esc(p.alias)} · ${esc(p.tag)}</p>
          </div>
        </div>
        <p class="headline">${esc(p.headline)}</p>
        <dl class="metrics">
          ${p.metrics.map((m) => `<div><dt>${esc(m.k)}</dt><dd>${esc(m.v)}</dd></div>`).join('')}
        </dl>
      </article>`
    ).join('');
    return `<section class="grid4">${cards}</section>
      <section class="strip">
        <div><b>同一个问题，四种答案</b>
          <span>occ 用生成器把「事件流」变成 loop 的边界；codex 用 typed Op/EventMsg 把「输入」变成 loop 的边界；
          zcode 用进程把「崩溃域」变成 loop 的边界；我们的 loop 没有边界——决策、执行、持久都在里面。</span></div>
      </section>`;
  }

  /* ── 视图 2：四种 loop ── */
  function viewLoops() {
    return D.projects.map(
      (p) => `<article class="loop" style="--c:${p.color}">
        <header class="loop-h">
          <h2>${esc(p.name)}</h2>
          <span class="tag">${esc(p.tag)}</span>
        </header>
        <p class="headline">${esc(p.headline)}</p>
        <div class="flow-wrap">${flowSvg(p.flow, p.color)}</div>
        <div class="loop-body">
          <table class="facts">
            <tbody>
              ${p.facts.map((f) => `<tr>
                <th>${esc(f.k)}</th>
                <td>${esc(f.v)}${f.ref ? `<code>${esc(f.ref)}</code>` : ''}</td>
              </tr>`).join('')}
            </tbody>
          </table>
          <div class="two">
            <div class="steal"><h4>可以偷的</h4><ul>${p.steal.map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div>
            <div class="drop"><h4>不该抄的</h4><ul>${p.drop.map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div>
          </div>
        </div>
      </article>`
    ).join('');
  }

  /* ── 视图 3：矩阵 ── */
  function viewMatrix() {
    const head = `<thead><tr><th>维度</th>${D.projects.map((p) => `<th style="--c:${p.color}"><span class="swatch"></span>${esc(p.name)}</th>`).join('')}</tr></thead>`;
    const body = D.matrix.map((r) => `<tr>
      <th>${esc(r.dim)}</th>
      <td>${esc(r.occ)}</td><td>${esc(r.codex)}</td><td>${esc(r.zcode)}</td><td>${esc(r.qaqh)}</td>
    </tr>`).join('');
    return `<div class="table-wrap"><table class="matrix">${head}<tbody>${body}</tbody></table></div>`;
  }

  /* ── 视图 4：拆库路线 ── */
  function viewLadder() {
    const rows = D.ladder.steps.map((s) => `<tr data-step="${s.id}">
        <td class="n">${s.id}</td>
        <td><b>${esc(s.label)}</b><div class="detail">${esc(s.detail)}</div></td>
        <td class="num">−${num(s.remove)}</td>
        <td class="num">${num(s.remain)}</td>
        <td class="num">+${s.crates}</td>
        <td class="risk">${esc(s.risk)}</td>
      </tr>`).join('');

    const bars = [`<div class="bar-row base"><span class="lbl">今天</span>
        <div class="track"><div class="fill" style="width:100%;--c:#22d3ee"></div></div>
        <span class="val">${num(D.ladder.total)}</span></div>`]
      .concat(D.ladder.steps.map((s) => `<div class="bar-row" data-step="${s.id}">
        <span class="lbl">L${s.id}</span>
        <div class="track"><div class="fill" style="width:${pct(s.remain)};--c:#22d3ee"></div></div>
        <span class="val">${num(s.remain)}</span></div>`)).join('');

    const sumOf = (b) => D.peel.filter((m) => m.batch === b).reduce((a, m) => a + m.loc, 0);
    const peelRows = D.peel.map((m) => `<tr><td><span class="batch ${m.batch.toLowerCase()}">${esc(m.batch)}</span></td>
      <td><code>${esc(m.module)}</code></td><td class="num">${num(m.loc)}</td>
      <td>${esc(m.to)}</td><td class="detail">${esc(m.note)}</td></tr>`).join('');

    return `<section class="ladder-top">
        <div class="ladder-copy">
          <h2>45,029 → ${num(D.ladder.final)}</h2>
          <p>拆的不是文件，是<strong>所有权</strong>。每一刀都要求：cargo 依赖边可验证、产品行为不变、旧 wire 不动。
          前三刀不需要碰 loop 一行。</p>
          <div class="crates">
            <span><b>${D.crates.current}</b> 现 crate</span>
            <span><b>${D.crates.packages}</b> 现 workspace 包</span>
            <span><b>${D.crates.target}</b> 目标 crate</span>
            <span><b>${D.crates.missing}</b> 缺口</span>
          </div>
          <p class="note">${esc(D.crates.note)}</p>
        </div>
        <div class="ladder-bars" id="bars">${bars}</div>
      </section>
      <section class="ladder-actions">
        <button id="next-cut" class="btn">执行下一刀 ▸</button>
        <button id="reset-cut" class="btn ghost">重置</button>
        <span id="cut-hint" class="hint">当前：L0（基线 45,029 行）</span>
      </section>
      <div class="table-wrap"><table class="cuts">
        <thead><tr><th>#</th><th>动作</th><th>移除</th><th>剩余</th><th>新 crate</th><th>风险</th></tr></thead>
        <tbody>${rows}</tbody>
      </table></div>
      <h3 class="sub-h">先剥谁：L1 的 8 个模块共 ${num(sumOf('L1'))} 行，对 <code>crate::agent</code> 引用为 0；L3 再带走 ${num(sumOf('L3'))} 行（registry 另计 3,009）</h3>
      <div class="table-wrap"><table class="cuts">
        <thead><tr><th>批次</th><th>模块</th><th>行数</th><th>去向</th><th>说明</th></tr></thead>
        <tbody>${peelRows}</tbody>
        <tfoot><tr><td></td><th>合计</th><td class="num">${num(sumOf('L1') + sumOf('L3'))}</td><td colspan="2" class="detail">L1 ${num(sumOf('L1'))} + L3 ${num(sumOf('L3'))}</td></tr></tfoot>
      </table></div>`;
  }

  function wireLadder() {
    const bars = document.getElementById('bars');
    if (!bars) return;
    const rows = Array.from(bars.querySelectorAll('.bar-row'));
    const hint = document.getElementById('cut-hint');
    let shown = 0;
    const paint = () => {
      rows.forEach((r) => {
        const step = Number(r.dataset.step || 0);
        r.classList.toggle('visible', step === 0 || step <= shown);
        r.classList.toggle('now', step === shown);
      });
      const s = D.ladder.steps[shown - 1];
      hint.textContent = shown === 0
        ? '当前：L0（基线 45,029 行）'
        : `当前：L${s.id} · ${s.label} · 剩余 ${num(s.remain)} 行`;
    };
    document.getElementById('next-cut').addEventListener('click', () => {
      shown = Math.min(shown + 1, D.ladder.steps.length);
      paint();
    });
    document.getElementById('reset-cut').addEventListener('click', () => { shown = 0; paint(); });
    paint();
  }

  /* ── 视图 5：结论 ── */
  function viewConclusion() {
    return `<section class="concl">
      ${D.conclusion.map((c, i) => `<article class="cc" style="--i:${i}">
        <span class="idx">${String(i + 1).padStart(2, '0')}</span>
        <h3>${esc(c.h)}</h3><p>${esc(c.p)}</p>
      </article>`).join('')}
    </section>
    <section class="nextbox">
      <h3>下一步</h3>
      <ol>${D.next.map((n) => `<li>${esc(n)}</li>`).join('')}</ol>
    </section>`;
  }

  const VIEWS = { overview: viewOverview, loops: viewLoops, matrix: viewMatrix, ladder: viewLadder, conclusion: viewConclusion };

  function go(id) {
    renderTabs(id, go);
    view.innerHTML = VIEWS[id]();
    if (id === 'ladder') wireLadder();
    window.scrollTo({ top: 0, behavior: 'smooth' });
    if (history.replaceState) history.replaceState(null, '', '#' + id);
  }

  renderMeta();
  const start = (location.hash || '').replace('#', '');
  go(VIEWS[start] ? start : 'overview');
})();
