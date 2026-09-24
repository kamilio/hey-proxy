/* Local dashboard. No CDN, telemetry, or browser credentials outside /logs. */
(function () {
  'use strict';
  const book = typeof module !== 'undefined' && module.exports ? require('./logs/prices.json') : window.HEY_PROXY_PRICES;
  const PRICE_SOURCE = book.source, PRICE_DATE = book.version, PRICES = book.prices, PRICE_ALIASES = book.aliases;
  const valid = n => typeof n === 'number' && Number.isFinite(n) && n >= 0;
  const destination = e => e.routed_model || e.requested_model || '(no model)';
  const modelName = (e, group = 'requested') => group === 'destination' ? destination(e) : e.requested_model || e.routed_model || '(no model)';
  const hasUsage = e => valid(e.input_tokens) || valid(e.output_tokens);
  const sum = (data, key) => data.reduce((n, e) => n + (valid(e[key]) ? e[key] : 0), 0);
  function priceModel(name) {
    name = PRICE_ALIASES[name] || name;
    return Object.hasOwn(PRICES, name) ? name : null;
  }
  function estimate(e) {
    // A public destination takes precedence (a smaller destination uses its own rates).
    // For private destinations use the regular requested name from the alias.
    const model = priceModel(e.routed_model) || (e.routed_model?.startsWith('gemini/') ? null : priceModel(e.requested_model));
    const eligible = (e.method === 'POST' || e.method === 'SEND') &&
      ['/v1/responses', '/v1/responses/compact', '/v1/chat/completions', '/v1/completions'].includes(e.path);
    if (!model || !eligible || !valid(e.input_tokens) || !valid(e.output_tokens)) return {model, usd: null};
    const [input, cache, output, write, threshold] = PRICES[model];
    const cached = valid(e.cached_input_tokens) ? e.cached_input_tokens : 0;
    const writes = valid(e.cache_write_tokens) ? e.cache_write_tokens : 0;
    if (cached + writes > e.input_tokens) return {model, usd: null};
    const long = threshold && e.input_tokens > threshold;
    return {model, usd: ((e.input_tokens - cached - writes) * input + cached * (cache ?? input) + writes * (write ?? input)) * (long ? 2 : 1) / 1e6 + e.output_tokens * output * (long ? 1.5 : 1) / 1e6};
  }
  function percentile(values, p) {
    if (!values.length) return null;
    const sorted = [...values].sort((a, b) => a - b);
    return sorted[Math.max(0, Math.ceil(p * sorted.length) - 1)];
  }
  function throughput(e) {
    if(e.state!=='succeeded'||!valid(e.total_duration_ms)||e.total_duration_ms<=0||!valid(e.output_tokens)||e.output_tokens<=0)return {output:null,visible:null};
    const output=e.output_tokens*1000/e.total_duration_ms;
    const visible=valid(e.reasoning_tokens)&&e.reasoning_tokens<=e.output_tokens?(e.output_tokens-e.reasoning_tokens)*1000/e.total_duration_ms:null;
    return {output,visible};
  }
  function summarize(data, minutes = 1) {
    const measured = data.filter(e => valid(e.status) && e.status >= 100);
    const errors = measured.filter(e => e.status >= 400).length;
    const timings = data.map(e => e.duration_ms).filter(valid);
    const costs = data.map(estimate).filter(e => e.usd != null);
    const speeds=data.map(throughput),complete=data.filter(e=>e.state==='succeeded');
    const reported = data.filter(hasUsage).length;
    return {
      requests: data.length, rate: data.length / minutes,
      outputRate:percentile(speeds.map(s=>s.output).filter(valid),.5),
      visibleRate:percentile(speeds.map(s=>s.visible).filter(valid),.5),
      speedSamples:speeds.filter(s=>s.output!=null).length,
      visibleSamples:speeds.filter(s=>s.visible!=null).length,
      firstOutput:percentile(complete.map(e=>e.first_output_ms).filter(valid),.5),
      totalDuration:percentile(complete.map(e=>e.total_duration_ms).filter(valid),.5),
      input: data.some(e => valid(e.input_tokens)) ? sum(data, 'input_tokens') : null,
      output: data.some(e => valid(e.output_tokens)) ? sum(data, 'output_tokens') : null,
      cached: data.some(e => valid(e.cached_input_tokens)) ? sum(data, 'cached_input_tokens') : null,
      total: reported ? sum(data, 'input_tokens') + sum(data, 'output_tokens') : null,
      coverage: data.length ? reported / data.length * 100 : null, reported,
      success: measured.length ? (measured.length - errors) / measured.length * 100 : null,
      error: measured.length ? errors / measured.length * 100 : null, errors,
      measured: measured.length, pending: data.length - measured.length,
      median: percentile(timings, .5), p95: percentile(timings, .95),
      retries: sum(data, 'retries'), retried: data.filter(e => valid(e.retries) && e.retries > 0).length,
      spend: costs.length ? costs.reduce((n, e) => n + e.usd, 0) : null, priced: costs.length,
    };
  }
  function filterEntries(entries, {end, minutes, selected = new Set(), includeClients = false, model = '', group = 'requested'}) {
    const start = end - minutes * 60000;
    return entries.filter(e => valid(e.timestamp_ms) && e.timestamp_ms >= start && e.timestamp_ms <= end &&
      (!selected.size || selected.has(e.machine)) && (includeClients || e.mode !== 'client') && (!model || modelName(e, group) === model));
  }
  function buildSeries(data, start, end, group = 'requested', count = 30) {
    const width = (end - start) / count, groups = new Map();
    for (const e of data) {
      if (!valid(e.timestamp_ms) || e.timestamp_ms < start || e.timestamp_ms > end) continue;
      const name = modelName(e, group);
      if (!groups.has(name)) groups.set(name, Array.from({length: count}, () => []));
      groups.get(name)[Math.min(count - 1, Math.floor((e.timestamp_ms - start) / width))].push(e);
    }
    return [...groups].sort(([a], [b]) => a.localeCompare(b)).map(([name, buckets]) => ({
      name, points: buckets.map(bucket => summarize(bucket, width / 60000)),
    }));
  }
  function smooth(values, weight) {
    let previous = null;
    return values.map(value => {
      if (value == null) { previous = null; return null; }
      previous = previous == null ? value : previous * weight + value * (1 - weight);
      return previous;
    });
  }
  const METRICS = [
    ['traffic/requests', 'requests', 'count', 'Observed requests in each time bucket.'],
    ['traffic/requests_per_minute', 'rate', 'rate', 'Request count divided by bucket duration in minutes.'],
    ['spend/estimated_usd', 'spend', 'usd', 'Standard list-price estimate for requests with input and output usage.'],
    ['tokens/input', 'input', 'count', 'Reported input tokens, including cached input.'],
    ['tokens/output', 'output', 'count', 'Reported output tokens, including reasoning when reported.'],
    ['tokens/total', 'total', 'count', 'Reported input + output tokens; partial usage can undercount.'],
    ['tokens/cached_input', 'cached', 'count', 'Reported cache reads, a subset of input tokens.'],
    ['tokens/usage_coverage', 'coverage', 'pct', 'Requests reporting at least one token field / all requests.'],
    ['health/success_rate', 'success', 'pct', 'Known statuses below 400 / all known statuses; excludes pending.'],
    ['health/error_rate', 'error', 'pct', 'Known statuses 400 or higher / all known statuses.'],
    ['health/retry_attempts', 'retries', 'count', 'Additional upstream attempts, including eventually successful requests.'],
    ['speed/output_tokens_per_second', 'outputRate', 'tps', 'Median completed output tokens / total request seconds, including reasoning tokens, waiting and retries.'],
    ['speed/visible_tokens_per_second', 'visibleRate', 'tps', 'Median (output minus reported reasoning tokens) / total request seconds. Requires reported reasoning-token usage.'],
    ['timing/median_ms', 'median', 'ms', 'Median time to HTTP headers (including recovery) / first WebSocket send.'],
    ['timing/p95_ms', 'p95', 'ms', '95th percentile time to headers / send; nearest-rank method.'],
  ].map(([id, key, unit, desc]) => ({id, key, unit, desc, category: id.split('/')[0]}));
  if (typeof module !== 'undefined' && module.exports) module.exports = {PRICES, PRICE_DATE, PRICE_SOURCE, estimate, priceModel, summarize, filterEntries, buildSeries, smooth, percentile, throughput, METRICS};
  if (typeof document === 'undefined') return;

  const $ = id => document.getElementById(id);
  const palette = ['#377fba', '#de8050', '#759749', '#aa72b0', '#3c9e91', '#d19a37', '#cb6983', '#8093ab'];
  const state = {entries: [], machines: [], selected: new Set(), end: Date.now(), paused: false, busy: false, loaded: false, stale: false, category: '', smoothing: 0, view: 'overview', modal: null, limit: 500, signature: '', bucket: -1};
  const compact = new Intl.NumberFormat(undefined, {notation: 'compact', maximumFractionDigits: 1});
  const exact = new Intl.NumberFormat(undefined, {maximumFractionDigits: 2});
  const time = t => new Date(t).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'});
  const money = n => n == null ? '—' : new Intl.NumberFormat('en-US', {style:'currency', currency:'USD', minimumFractionDigits:2, maximumFractionDigits:n > 0 && n < .01 ? 5 : 2}).format(n);
  function format(n, unit = 'count', raw = false) {
    if (n == null) return '—';
    if (unit === 'usd') return money(n);
    if (unit === 'pct') return `${exact.format(n)}%`;
    if (unit === 'ms') return n >= 1000 && !raw ? `${exact.format(n / 1000)} s` : `${exact.format(n)} ms`;
    if (unit === 'tps') return `${exact.format(n)} tok/s`;
    if (unit === 'rate') return `${exact.format(n)} /m`;
    return (raw || n < 10000 ? exact : compact).format(n);
  }
  function node(tag, text = '', cls = '') { const n = document.createElement(tag); n.textContent = text; if (cls) n.className = cls; return n; }
  function button(text, action, cls = '') { const b = node('button', text, cls); b.type = 'button'; b.onclick = action; return b; }
  function svgMark(root, tag, attrs = {}, text) {
    const n = document.createElementNS('http://www.w3.org/2000/svg', tag);
    for (const [key, value] of Object.entries(attrs)) n.setAttribute(key, value);
    if (text != null) n.textContent = text;
    root.append(n); return n;
  }
  const colors = new Map();
  function color(name) {
    if (!colors.has(name)) colors.set(name, palette[colors.size] || `hsl(${colors.size * 137.5 % 360} 48% 52%)`);
    return colors.get(name);
  }
  function options() { return {end: state.end, minutes: Number($('window').value), selected:state.selected, includeClients:$('clients').checked, model:$('model').value, group:$('group').value}; }
  let current = {data:[], series:[], start:state.end-3600000, end:state.end, summary:summarize([])};
  function machineButtons() {
    const signature = JSON.stringify([state.machines, [...state.selected]]);
    if ($('machines').dataset.signature === signature) return;
    $('machines').dataset.signature = signature;
    const all = button('All machines', () => {state.selected.clear(); render();});
    all.setAttribute('aria-pressed', state.selected.size === 0);
    $('machines').replaceChildren(all);
    for (const m of state.machines) {
      const b = button('', () => {state.selected.has(m.name) ? state.selected.delete(m.name) : state.selected.add(m.name); render();});
      b.setAttribute('aria-pressed', state.selected.has(m.name));
      const dot = node('span', '', 'dot'); dot.style.background = m.status === 'live' ? 'var(--good)' : m.status === 'unavailable' ? 'var(--bad)' : 'var(--muted)';
      b.append(dot, document.createTextNode(`${m.name === 'local' ? 'This machine' : m.name} · ${m.status}${m.mode === 'client' ? ' · relay' : ''}`));
      $('machines').append(b);
    }
  }
  function modelOptions() {
    const names = [...new Set(state.entries.map(e => modelName(e, $('group').value)))].sort();
    const signature = JSON.stringify([$('group').value, names]);
    if ($('model').dataset.signature === signature) return;
    const previous = $('model').value;
    $('model').replaceChildren(new Option('All models', ''), ...names.map(n => new Option(n, n)));
    $('model').value = names.includes(previous) ? previous : '';
    $('model').dataset.signature = signature;
  }
  function legend(root) {
    root.replaceChildren();
    for (const s of current.series) {const label = node('span'), swatch = node('i', '', 'swatch'); swatch.style.background = color(s.name); label.append(swatch, document.createTextNode(s.name)); root.append(label);}
  }
  const tooltip = $('chart-tooltip');
  let chartHover = null;
  function hideTip() {tooltip.hidden=true;chartHover=null;}
  function showTip(text, x, y) {
    tooltip.textContent = text; tooltip.hidden = false;
    const box = tooltip.getBoundingClientRect();
    tooltip.style.left = Math.max(12, Math.min(x + 12, innerWidth - box.width - 12)) + 'px';
    tooltip.style.top = Math.max(12, Math.min(y + 12, innerHeight - box.height - 12)) + 'px';
  }
  function lineChart(svg, metric) {
    svg.replaceChildren();
    const w = Math.max(240, svg.clientWidth || 400), h = svg.clientHeight || 175;
    const l = metric.unit === 'usd' ? 57 : 48, r = 13, top = 18, bottom = 27, pw = w - l - r, ph = h - top - bottom;
    svg.setAttribute('viewBox', `0 0 ${w} ${h}`); svg.setAttribute('aria-label', `${metric.id} by ${$('group').selectedOptions[0].textContent}. Use arrow keys for bucket values.`); svg.setAttribute('tabindex', '0');
    svgMark(svg, 'title', {}, `${metric.id}: ${metric.desc}`);
    const values = current.series.map(s => ({name:s.name, raw:s.points.map(p => p[metric.key])}));
    const maximum = Math.max(0, ...values.flatMap(s => s.raw.filter(v => v != null))) || 1;
    const max = metric.unit === 'pct' ? 100 : maximum;
    for (let i = 0; i < 4; i++) {const y = top + ph - i * ph / 3; svgMark(svg, 'line', {x1:l, y1:y, x2:w-r, y2:y, class:'grid'}); svgMark(svg, 'text', {x:l-7, y:y+3, 'text-anchor':'end'}, format(max*i/3, metric.unit));}
    const ticks = w < 400 ? 2 : 3;
    for (let i = 0; i <= ticks; i++) svgMark(svg, 'text', {x:l+i*pw/ticks, y:h-5, 'text-anchor':i===0?'start':i===ticks?'end':'middle'}, time(current.start+(current.end-current.start)*i/ticks));
    for (const s of values) {
      const points = smooth(s.raw, state.smoothing); let path = '', active = false;
      points.forEach((v,i) => {if (v == null) {active=false; return;} const x=l+(i+.5)*pw/30, y=top+ph-v/max*ph; path += `${active?'L':'M'}${x.toFixed(2)},${y.toFixed(2)} `; active=true;});
      svgMark(svg, 'path', {d:path, fill:'none', stroke:color(s.name), 'stroke-width':1.6, 'stroke-linejoin':'round'});
      points.forEach((v,i) => {if (v != null) svgMark(svg, 'circle', {cx:l+(i+.5)*pw/30, cy:top+ph-v/max*ph, r:1.8, fill:color(s.name)});});
    }
    if (!values.some(s => s.raw.some(v => v != null))) svgMark(svg, 'text', {x:l+pw/2, y:top+ph/2, 'text-anchor':'middle', class:'chart-empty'}, state.loaded ? 'No measurements in this window' : 'Waiting for snapshot…');
    let index = 29;
    const tip = (i,x,y) => {index=i; chartHover={svg,index:i,x,y}; const width=(current.end-current.start)/30; showTip(`${time(current.start+i*width)} – ${time(current.start+(i+1)*width)} · raw\n${values.map(s=>`${s.name}: ${format(s.raw[i],metric.unit,true)}`).join('\n') || 'No measurements'}`,x,y);};
    svg.updateTip=tip;
    svg.onpointermove = e => {const b=svg.getBoundingClientRect();tip(Math.max(0,Math.min(29,Math.floor(((e.clientX-b.left)/b.width*w-l)/pw*30))),e.clientX,e.clientY);};
    svg.onpointerleave = svg.onblur = hideTip;
    svg.onfocus = () => {const b=svg.getBoundingClientRect();tip(index,b.left+b.width/2,b.top+30);};
    svg.onkeydown = e => {if(e.key==='Escape'){hideTip();return;}if(!['ArrowLeft','ArrowRight','Home','End'].includes(e.key))return;e.preventDefault();index=e.key==='Home'?0:e.key==='End'?29:Math.max(0,Math.min(29,index+(e.key==='ArrowRight'?1:-1)));const b=svg.getBoundingClientRect();tip(index,b.left+b.width/2,b.top+30);};
  }
  function cards(root, metrics) {
    // Keep buttons/focus stable while values refresh.
    if (root.dataset.keys !== metrics.map(m=>m.id).join(',')) {
      root.replaceChildren(); root.dataset.keys=metrics.map(m=>m.id).join(',');
      for (const m of metrics) {
        const card=node('article','','card'), head=node('div','','metric-head'), value=node('div','','metric-value');
        head.append(button(m.id,()=>openModal(m),'metric-open'),value);
        card.append(head,node('p',m.desc,'metric-desc'));
        svgMark(card,'svg',{class:'chart',role:'img'}); root.append(card);
      }
    }
    metrics.forEach((m,i)=>{const card=root.children[i], value=card.querySelector('.metric-value');value.replaceChildren(document.createTextNode(format(current.summary[m.key],m.unit)),node('small','window'));lineChart(card.querySelector('svg'),m);});
  }
  function tableEmpty(root, span, text) {const tr=node('tr'),td=node('td',text,'empty');td.colSpan=span;tr.append(td);root.append(tr);}
  function rowCells(root, values, numericFrom) {const row=node('tr');values.forEach((v,i)=>row.append(node('td',v ?? '—',i>=numericFrom?'num':'')));root.append(row);return row;}
  function perModel() {
    const root=$('model-totals'); root.replaceChildren();
    for(const s of current.series) {
      const data=current.data.filter(e=>modelName(e,$('group').value)===s.name), a=summarize(data);
      const rates=[...new Set(data.map(e=>estimate(e).model).filter(Boolean))];
      const row=rowCells(root,[s.name,rates.join(', ')||'Unpriced',format(a.requests),format(a.input),format(a.output),money(a.spend),format(a.success,'pct'),format(a.median,'ms'),format(a.retries)],2);
      const pick=button(s.name,()=>{$('model').value=s.name;render();},'metric-open'); pick.style.color=color(s.name);row.firstChild.replaceChildren(pick);
      row.children[5].title=`${a.priced} / ${a.requests} requests priced; estimate at standard list rates`;
      row.children[5].append(node('small',`${a.priced} / ${a.requests} priced`,'price-coverage'));
    }
    if(!current.series.length)tableEmpty(root,9,'No model traffic in this window.');
  }
  function renderOverview() {
    const s=current.summary;
    for(const [id,key,unit]of [['requests','requests'],['tokens','total'],['success','success','pct'],['latency','median','ms'],['retries','retries'],['spend','spend','usd']])$(id).textContent=format(s[key],unit);
    $('coverage').textContent=`${s.reported} / ${s.requests} requests report usage`;
    $('spend-note').textContent=`${s.priced} / ${s.requests} requests priced`;
    $('request-note').textContent=`${s.pending} pending · ${s.errors} error statuses`;
    $('retry-note').textContent=`${s.retried} requests retried`;
    $('scope-label').textContent=state.selected.size?[...state.selected].join(', '):'all machines';
    $('history-note').textContent=$('window').selectedOptions[0].textContent + ' · retained history';
    $('group-note').textContent=` / by ${$('group').selectedOptions[0].textContent.toLowerCase()}`;
    legend($('overview-legend'));
    cards($('overview-charts'),['rate','spend','total','success','median','retries'].map(key=>METRICS.find(m=>m.key===key)));
    perModel();
    $('model-share').replaceChildren();
    const shares=current.series.map(s=>({name:s.name,count:s.points.reduce((n,p)=>n+p.requests,0)})).sort((a,b)=>b.count-a.count);
    for(const s of shares){const row=node('div','','share-row'),track=node('div','','track'),bar=node('i');bar.style.cssText=`width:${s.count/current.data.length*100}%;background:${color(s.name)}`;track.append(bar);const name=node('span',s.name);name.title=s.name;row.append(name,track,node('span',format(s.count),'num'));$('model-share').append(row);}
    if(!shares.length)$('model-share').append(node('p','No traffic in this window.','empty'));
    $('activity').replaceChildren();
    for(const e of [...current.data].sort((a,b)=>b.timestamp_ms-a.timestamp_ms).slice(0,12)){const row=node('div','','feed-row'),name=node('div',`${e.requested_model||'(no model)'} → ${destination(e)}`,'feed-model');name.title=name.textContent;name.append(node('small',`${e.machine||'local'} · ${e.method||''} ${e.path||''}`));row.append(node('span',time(e.timestamp_ms),'muted'),name,node('span',e.status??'pending',e.status>=400?'bad':''));$('activity').append(row);}
    if(!current.data.length)$('activity').append(node('p','Waiting for requests.','empty'));
    volume(); routes();
  }
  function volume() {
    const svg=$('chart'), key=$('metric').value, unit=key==='spend'?'usd':'count'; svg.replaceChildren();
    const w=Math.max(240,svg.clientWidth),h=245,l=60,pw=w-l-15,ph=185,top=18;
    svg.setAttribute('viewBox',`0 0 ${w} ${h}`);
    const totals=Array.from({length:30},(_,i)=>current.series.reduce((n,s)=>n+(s.points[i][key]||0),0));
    const max=Math.max(1,...totals), any=current.series.some(s=>s.points.some(p=>p[key]!=null));
    $('volume-empty').hidden=any;$('volume-empty').textContent=key==='requests'?'No requests in this window.':`No ${key==='spend'?'priced usage':'reported token usage'} in this window.`;
    svg.hidden=!any;
    const width=(current.end-current.start)/30;
    $('bucket-note').textContent=`${format(width/60000)} min buckets`;
    $('chart-note').textContent=`${$('metric').selectedOptions[0].textContent} stacked by ${$('group').selectedOptions[0].textContent.toLowerCase()}${key!=='requests'?' · unavailable usage excluded':''}`;
    for(let i=0;i<4;i++){const y=top+ph-i*ph/3;svgMark(svg,'line',{x1:l,y1:y,x2:w-15,y2:y,class:'grid'});svgMark(svg,'text',{x:l-8,y:y+3,'text-anchor':'end'},format(max*i/3,unit));}
    for(let i=0;i<30;i++){let base=0;for(const s of current.series){const value=s.points[i][key];if(!value)continue;const rect=svgMark(svg,'rect',{x:l+i*pw/30+1,y:top+ph-(base+value)/max*ph,width:Math.max(1,pw/30-2),height:value/max*ph,fill:color(s.name)});const text=`${time(current.start+i*width)} – ${time(current.start+(i+1)*width)}\n${s.name}: ${format(value,unit,true)}`;svgMark(rect,'title',{},text);rect.onpointermove=e=>showTip(text,e.clientX,e.clientY);base+=value;}}
    for(let i=0;i<=3;i++)svgMark(svg,'text',{x:l+i*pw/3,y:h-8,'text-anchor':i===0?'start':i===3?'end':'middle'},time(current.start+i*(current.end-current.start)/3));
    svg.onpointerleave=()=>{tooltip.hidden=true;};
  }
  const effortLabel = (e, key) => e.route_rule ? e[key] || 'not supplied' : 'not recorded';
  function routes() {
    const data=current.data.filter(e=>e.requested_model),groups=new Map();
    for(const e of data){const key=JSON.stringify([e.machine,e.requested_model,destination(e),e.project,e.requested_reasoning,e.routed_reasoning,e.route_rule]);if(!groups.has(key))groups.set(key,[]);groups.get(key).push(e);}
    const rewritten=data.filter(e=>destination(e)!==e.requested_model).length,known=data.filter(e=>e.route_rule).length;
    $('rewrite-count').textContent=`${format(rewritten,'count',true)} / ${format(data.length,'count',true)} model names rewritten`;
    const destinations=new Map();for(const e of data)destinations.set(destination(e),(destinations.get(destination(e))||0)+1);
    $('rewrite-summary').replaceChildren(...[...destinations].sort((a,b)=>b[1]-a[1]).map(([name,count])=>node('span',`${name} · ${format(count)} · ${format(count/data.length*100,'pct')}`)));
    $('rows').replaceChildren();
    for(const entries of [...groups.values()].sort((a,b)=>b.length-a.length)){const e=entries[0];rowCells($('rows'),[e.machine,`${e.requested_model} → ${destination(e)}`,effortLabel(e,'requested_reasoning'),effortLabel(e,'routed_reasoning'),e.route_rule==='reasoning'?'effort match':e.route_rule==='alias'?'alias fallback':e.route_rule||'not recorded',e.project||'—',format(entries.length,'count',true),format(entries.length/data.length*100,'pct')],6);}
    if(!groups.size)tableEmpty($('rows'),8,'No model requests in this window.');
    $('reasoning-note').textContent=`${format(known)} / ${format(data.length)} requests have routing evidence. Older records have no effort information; their destination is still shown. Counts describe this selection, not rule predictions.`;
    renderSpeed();routingConfig();
  }
  function renderSpeed() {
    const groups=new Map();
    for(const e of current.data){if(!e.requested_model)continue;const key=JSON.stringify([destination(e),effortLabel(e,'requested_reasoning')]);if(!groups.has(key))groups.set(key,[]);groups.get(key).push(e);}
    const root=$('speed-rows');root.replaceChildren();
    for(const data of [...groups.values()].sort((a,b)=>destination(a[0]).localeCompare(destination(b[0]))||effortLabel(a[0],'requested_reasoning').localeCompare(effortLabel(b[0],'requested_reasoning')))){
      const e=data[0],s=summarize(data);
      rowCells(root,[destination(e),effortLabel(e,'requested_reasoning'),format(s.speedSamples,'count',true),format(s.outputRate,'tps'),format(s.visibleRate,'tps'),format(s.firstOutput,'ms'),format(s.totalDuration,'ms')],2);
    }
    if(!groups.size)tableEmpty(root,7,'No model requests in this window.');
  }
  function routingConfig() {
    const machines=state.machines.filter(m=>!state.selected.size||state.selected.has(m.name));
    const signature=JSON.stringify(machines.map(m=>[m.name,m.status,m.routing]));
    const root=$('routing-config');if(root.dataset.signature===signature)return;root.dataset.signature=signature;root.replaceChildren();
    for(const m of machines){const section=node('section','','config-machine'),config=m.routing;
      section.append(node('h3',`${m.name==='local'?'This machine':m.name} · ${m.mode||m.status}`));
      if(!config){section.append(node('p',m.status==='live'?'Config unavailable from this proxy version. Update it to expose active rules.':'Waiting for this machine’s config.','sub'));root.append(section);continue;}
      if(config.mode==='client'){section.append(node('p','Client relay: model rewrites are applied by its host.','sub'));root.append(section);continue;}
      section.append(node('p',`Default key project: ${config.default_project} · models without an alias pass through unchanged`,'sub'));
      const table=node('table'),head=node('thead'),body=node('tbody'),wrap=node('div','','table-wrap');rowCells(head,['Requested model','Incoming effort condition','Destination model','Sent effort','Key project'],99);table.append(head,body);wrap.append(table);
      for(const a of config.aliases||[]){for(const [effort,r]of Object.entries(a.reasoning_routes||{}))rowCells(body,[a.from,`effort = ${effort}`,r.to,a.reasoning||'preserve incoming',r.api_key||a.api_key||config.default_project],99);rowCells(body,[a.from,Object.keys(a.reasoning_routes||{}).length?'otherwise / effort not supplied':'any effort',a.to||a.from,a.reasoning||'preserve incoming',a.api_key||config.default_project],99);}
      if(!config.aliases?.length)tableEmpty(body,5,'No aliases configured. Models pass through unchanged.');
      section.append(wrap);const details=node('details'),pre=node('pre',JSON.stringify(config,null,2));details.append(node('summary','View config JSON · credentials omitted'),pre);section.append(details);root.append(section);
    }
  }
  function coverageText() {
    return state.machines.filter(m=>(!state.selected.size||state.selected.has(m.name))&&($('clients').checked||m.mode!=='client')).map(m=>{
      const c=m.coverage,label=m.name==='local'?'This machine':m.name;
      if(m.status!=='live')return `${label}: ${m.status}`;
      if(c?.source==='sqlite')return `${label}: ${c.truncated?`partial · newest ${format(c.limit)} records; use Reports for full totals`:'retained SQLite history'}${c.earliest_ms>current.start?' · collection starts '+new Date(c.earliest_ms).toLocaleString():''}`;
      return `${label}: partial live sample · latest ${c?.limit||500} requests; update this proxy for historical windows`;
    }).join(' · ');
  }
  function renderMetrics() {
    const query=$('metric-search').value.toLowerCase().trim();
    const metrics=METRICS.filter(m=>(!state.category||m.category===state.category)&&`${m.id} ${m.desc}`.toLowerCase().includes(query));
    $('metric-heading').textContent=state.category?`${state.category} /`:'All metrics';$('metric-count').textContent=`${metrics.length} metrics`;
    $('metrics-empty').hidden=metrics.length>0;
    for(const b of $('metric-tree').querySelectorAll('button'))b.setAttribute('aria-pressed',b.dataset.category===state.category);
    legend($('metrics-legend'));cards($('metrics-charts'),metrics);
  }
  function renderRequests() {
    const query=$('search').value.toLowerCase().trim();
    const data=current.data.filter(e=>[e.machine,e.requested_model,e.routed_model,e.requested_reasoning,e.routed_reasoning,e.route_rule,e.project,e.path,e.method,e.transport,e.status??'pending'].join(' ').toLowerCase().includes(query)).sort((a,b)=>b.timestamp_ms-a.timestamp_ms);
    $('log-count').textContent=`${format(data.length)} matching requests${data.length>500?' · showing newest 500; use Reports to page through all':''}`;$('log-rows').replaceChildren();
    for(const e of data.slice(0,500))rowCells($('log-rows'),[new Date(e.timestamp_ms).toLocaleTimeString(),e.machine,`${e.method||''} ${e.path||''}${e.transport==='WebSocket'?' · WS':''}`,e.requested_model,destination(e),effortLabel(e,'requested_reasoning'),effortLabel(e,'routed_reasoning'),e.route_rule||'not recorded',e.project,e.status??'Pending',e.retries??0,format(e.input_tokens),format(e.output_tokens),format(e.duration_ms,'ms'),money(estimate(e).usd)],9);
    if(!data.length)tableEmpty($('log-rows'),15,query?'No requests match this search.':'No requests in this window.');
  }
  function openModal(metric) {
    state.modal=metric;
    // Tooltips belong inside the native dialog's top layer while it is open.
    $('metric-modal').append(tooltip);
    $('metric-modal').showModal();renderModal();
  }
  function renderModal() {
    if(!state.modal)return;const m=state.modal;
    $('modal-title').textContent=m.id;$('modal-desc').textContent=m.desc;
    $('modal-summary').textContent=`Window: ${format(current.summary[m.key],m.unit,true)} · ${current.data.length} requests · ${current.series.length} model series${m.key==='spend'?` · ${current.summary.priced} requests priced`:''}${state.smoothing?` · smoothing ${state.smoothing}`:''}`;
    legend($('modal-legend'));lineChart($('modal-chart'),m);
  }
  function render() {
    tooltip.hidden=true;machineButtons();
    const o=options(),data=filterEntries(state.entries,o),start=o.end-o.minutes*60000;
    current={data,start,end:o.end,series:buildSeries(data,start,o.end,o.group),summary:summarize(data,o.minutes)};
    if(state.view==='overview')renderOverview();if(state.view==='metrics')renderMetrics();if(state.view==='requests')renderRequests();renderModal();
    if(chartHover && chartHover.svg.isConnected && chartHover.svg.getClientRects().length) {
      const {svg,index,x,y}=chartHover;svg.updateTip(index,x,y);
    } else chartHover=null;
    $('window-coverage').textContent=state.loaded?coverageText():'Loading selected time window…';
    $('summary').textContent=state.view==='reports'?'Historical reports · local SQLite archive':state.loaded?coverageText():'Loading selected time window…';
  }
  function route() {
    const view=location.hash.slice(1)||'overview';state.view=['overview','metrics','requests','reports','about'].includes(view)?view:'overview';
    for(const el of document.querySelectorAll('[data-view]')){if(el.dataset.view===state.view)el.setAttribute('aria-current','page');else el.removeAttribute('aria-current');}
    for(const view of ['overview','metrics','requests','reports','about'])$(`view-${view}`).hidden=state.view!==view;
    $('filters').hidden=['about','reports'].includes(state.view);$('live-heading').hidden=state.view==='reports';$('pause').hidden=state.view==='reports';render();if(state.view!=='reports')refresh();
  }
  function connection() {
    $('connection').textContent=state.paused?'Ⅱ Paused':state.stale?'Disconnected':'● Live';
    $('connection').className=state.stale?'connection bad':'connection';
  }
  let refreshSequence=0, controller;
  async function refresh(force=false) {
    if(state.paused||state.view==='reports'||state.busy&&!force)return;state.busy=true;
    const sequence=++refreshSequence;controller?.abort();controller=new AbortController();const activeController=controller,timeout=setTimeout(()=>activeController.abort(),20000);
    try {
      const response=await fetch('/logs/api?minutes='+$('window').value,{cache:'no-store',signal:controller.signal});
      if(!response.ok)throw Object.assign(new Error('Snapshot unavailable'),{status:response.status});
      const data=await response.json();if(!Array.isArray(data.entries))throw new Error('Invalid snapshot');if(state.paused||sequence!==refreshSequence)return;
      const now=Date.now(),machines=data.machines||[{name:'local',status:'live'}],signature=JSON.stringify([data.entries,machines]),bucket=Math.floor(now/(Number($('window').value)*60000/30));
      const changed=signature!==state.signature||bucket!==state.bucket||!state.loaded;
      state.entries=data.entries;state.machines=machines;state.end=now;state.limit=data.limit||500;state.loaded=true;state.stale=false;
      modelOptions();if(changed){state.signature=signature;state.bucket=bucket;render();}
      $('updated').textContent=`updated ${time(now)}`;$('notice').hidden=true;connection();
    } catch(error) {
      if(state.paused||sequence!==refreshSequence)return;state.stale=true;connection();$('notice').hidden=false;
      $('notice-text').textContent=error.status===401?'Dashboard access expired. Sign in again to resume.':`Could not refresh. ${state.loaded?'Showing the last successful snapshot.':'Waiting for the proxy.'} Retrying automatically.`;
      $('login-link').hidden=error.status!==401;
    } finally {clearTimeout(timeout);if(sequence===refreshSequence)state.busy=false;}
  }
  $('pause').onclick=()=>{state.paused=!state.paused;$('pause').textContent=state.paused?'Resume':'Pause';$('window').disabled=state.paused;connection();if(!state.paused)refresh();};
  $('theme').onclick=()=>{const theme=document.documentElement.dataset.theme==='dark'?'light':'dark';document.documentElement.dataset.theme=theme;try{localStorage.setItem('hey-proxy-theme',theme);}catch{}};
  $('window').onchange=()=>{state.entries=[];state.loaded=false;state.end=Date.now();render();refresh(true);};
  for(const id of ['model','clients','metric'])$(id).onchange=render;
  $('group').onchange=()=>{modelOptions();render();};
  $('search').oninput=renderRequests;$('metric-search').oninput=renderMetrics;
  $('smoothing').oninput=()=>{state.smoothing=Number($('smoothing').value);$('smoothing-value').textContent=state.smoothing?state.smoothing.toFixed(1):'off';render();};
  $('metric-tree').append(node('div','Metric browser','eyebrow'));
  for(const category of ['',...new Set(METRICS.map(m=>m.category))]){const b=button('',()=>{state.category=category;renderMetrics();});b.dataset.category=category;b.append(document.createTextNode(category||'all metrics'),node('span',METRICS.filter(m=>!category||m.category===category).length));$('metric-tree').append(b);}
  $('metric-tree').append(node('p','All charts share the selected machines, model, and time window. Click a metric name for details.'));
  $('modal-close').onclick=()=>$('metric-modal').close();
  $('metric-modal').addEventListener('close',()=>{state.modal=null;hideTip();});
  $('metric-modal').onclick=e=>{if(e.target===$('metric-modal')){const b=e.target.getBoundingClientRect();if(e.clientX<b.left||e.clientX>b.right||e.clientY<b.top||e.clientY>b.bottom)e.target.close();}};
  $('metric-modal').addEventListener('close',()=>document.body.append(tooltip));
  $('price-date').textContent=PRICE_DATE;
  const ratePrice=n=>n==null?'—':new Intl.NumberFormat('en-US',{style:'currency',currency:'USD',maximumFractionDigits:6}).format(n);
  for(const [model,p]of Object.entries(PRICES))rowCells($('price-rows'),[model,ratePrice(p[0]),ratePrice(p[1]),ratePrice(p[3]??p[0]),ratePrice(p[2])],1);
  window.addEventListener('hashchange',route);let resize;
  window.addEventListener('resize',()=>{clearTimeout(resize);resize=setTimeout(render,100);});
  window.addEventListener('scroll',hideTip,true);
  route();refresh();setInterval(refresh,5000);
})();
