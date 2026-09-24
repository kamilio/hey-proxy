const {test} = require('node:test');
const assert = require('node:assert/strict');
const {estimate, summarize, filterEntries, buildSeries, smooth, throughput} = require('../src/proxy/dashboard.js');
const entry = overrides => ({method:'POST', path:'/v1/responses', requested_model:'gpt-5.4', routed_model:'model-primary', input_tokens:100000, output_tokens:10000, timestamp_ms:1000, machine:'local', mode:'standalone', ...overrides});

test('Gemini overrides do not inherit OpenAI prices', () => {
  assert.equal(estimate(entry({routed_model:'gemini/unknown'})).usd, null);
  assert.equal(estimate(entry({routed_model:'gemini/unknown'})).model, null);
});
const near = (actual, expected) => assert.ok(Math.abs(actual-expected)<1e-10, `${actual} != ${expected}`);

test('Gemini model is estimated at Pro list rates with cached input and the 200k boundary', () => {
  for (const model of ['gemini/gemini-3.1-pro-preview','gemini/models/gemini-3.1-pro-preview','models/gemini-3.1-pro-preview']) {
    const base=entry({routed_model:model,input_tokens:200000,cached_input_tokens:100000,reasoning_tokens:6000});
    assert.equal(estimate(base).model,'gemini-3.1-pro-preview');
    near(estimate(base).usd,.34);
    near(estimate({...base,input_tokens:200001}).usd,.620004);
  }
});

test('private model aliases use the requested regular model, public destinations use their own price', () => {
  near(estimate(entry()).usd, .4);
  const routed = estimate(entry({routed_model:'gpt-4.1-mini'}));
  assert.equal(routed.model,'gpt-4.1-mini'); near(routed.usd,.056);
  assert.equal(estimate(entry({requested_model:'unknown',routed_model:'private'})).usd,null);
  assert.equal(estimate(entry({requested_model:'gpt-4.1-2025-04-14'})).model,'gpt-4.1');
  assert.equal(estimate(entry({requested_model:'o3-2025-04-16'})).model,'o3');
});
test('cache reads and writes are disjoint input categories, with long-context rates on the entire request', () => {
  near(estimate(entry({cached_input_tokens:60000,cache_write_tokens:20000})).usd, .265);
  near(estimate(entry({input_tokens:272000,output_tokens:10000})).usd,.83);
  near(estimate(entry({input_tokens:272001,output_tokens:10000})).usd,1.585005);
  near(estimate(entry({input_tokens:300000,cached_input_tokens:100000,cache_write_tokens:50000})).usd,1.275);
  assert.equal(estimate(entry({cached_input_tokens:100001})).usd,null);
  near(estimate(entry({input_tokens:0,output_tokens:0})).usd,0);
});
test('missing usage is unpriced; retrievals and unsupported endpoints do not count as generation spend', () => {
  for(const overrides of [{input_tokens:null},{output_tokens:null},{method:'GET'},{path:'/v1/images/generations'}])assert.equal(estimate(entry(overrides)).usd,null);
  assert.equal(estimate(entry({method:'SEND',transport:'WebSocket'})).usd,.4);
  const summary=summarize([entry(),entry({output_tokens:null}),entry({requested_model:'unknown'})]);
  assert.equal(summary.priced,1);near(summary.spend,.4);
  assert.equal(summarize([]).spend,null);
});
test('success excludes pending, timing handles zero, percentiles are nearest rank, and usage remains partial', () => {
  const data=[entry({status:200,duration_ms:0,retries:2}),entry({status:503,duration_ms:1000}),entry({status:null,duration_ms:null,input_tokens:null,output_tokens:null}),entry({status:101,duration_ms:2000,input_tokens:null})];
  const s=summarize(data,2);
  assert.equal(s.requests,4);assert.equal(s.pending,1);assert.equal(s.errors,1);near(s.success,200/3);
  assert.equal(s.median,1000);assert.equal(s.p95,2000);assert.equal(s.retries,2);assert.equal(s.retried,1);
  assert.equal(s.coverage,75);assert.equal(s.rate,2);assert.equal(s.input,200000);assert.equal(s.output,30000);
  const unknown=summarize([entry({input_tokens:null,output_tokens:null})]);
  assert.equal(unknown.total,null);assert.equal(unknown.success,null);assert.equal(unknown.median,null);
});
test('time, machine and model filters are shared and client relays are excluded by default', () => {
  const data=[entry(),entry({timestamp_ms:2000,machine:'remote'}),entry({timestamp_ms:3000,mode:'client'}),entry({timestamp_ms:4000}),entry({timestamp_ms:0}),entry({timestamp_ms:null})];
  const options={end:3000,minutes:2/60};
  assert.equal(filterEntries(data,options).length,2);
  assert.equal(filterEntries(data,{...options,includeClients:true}).length,3);
  assert.equal(filterEntries(data,{...options,selected:new Set(['remote'])}).length,1);
  assert.equal(filterEntries(data,{...options,model:'model-primary',group:'destination'}).length,2);
  assert.equal(filterEntries(data,{...options,model:'model-primary',group:'requested'}).length,0);
});
test('bucket endpoints do not lose or duplicate entries and unknown values form gaps', () => {
  const series=buildSeries([entry({timestamp_ms:0}),entry({timestamp_ms:1000}),entry({timestamp_ms:3000}),entry({timestamp_ms:3001})],0,3000,'requested',3);
  assert.deepEqual(series[0].points.map(p=>p.requests),[1,1,1]);
  assert.equal(series[0].name,'gpt-5.4');
  const gaps=buildSeries([entry({timestamp_ms:0})],0,3000,'destination',3)[0];
  assert.equal(gaps.name,'model-primary');assert.deepEqual(gaps.points.map(p=>p.spend),[.4,null,null]);
  assert.deepEqual(smooth([10,20,null,40,20],.5),[10,15,null,40,30]);
});

test('throughput uses successful completed lifetimes and separates reported reasoning', () => {
  const e=entry({state:'succeeded',output_tokens:300,reasoning_tokens:100,total_duration_ms:2000});
  assert.deepEqual(throughput(e),{output:150,visible:100});
  assert.deepEqual(throughput({...e,reasoning_tokens:null}),{output:150,visible:null});
  for(const overrides of [{state:'streaming'},{state:'failed'},{state:'cancelled'},{total_duration_ms:0},{total_duration_ms:null},{output_tokens:null}])assert.deepEqual(throughput({...e,...overrides}),{output:null,visible:null});
  assert.deepEqual(throughput({...e,reasoning_tokens:301}),{output:150,visible:null});
  const summary=summarize([e,{...e,output_tokens:600,first_output_ms:100},{...e,state:'streaming',output_tokens:900}]);
  assert.equal(summary.outputRate,150);assert.equal(summary.visibleRate,100);assert.equal(summary.speedSamples,2);assert.equal(summary.firstOutput,100);assert.equal(summary.totalDuration,2000);
});
