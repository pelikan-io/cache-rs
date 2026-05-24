# Weekly Cache Research Report — 2026-05-24

**Run type:** Normal weekly run. Prior report: [`weekly-cache-report-2026-05-17.md`](./weekly-cache-report-2026-05-17.md). Search horizon: 2026-05-18 → 2026-05-24 (the seven days following the prior report), with a deliberate sweep of the 2605.13xxx–2605.21xxx arXiv batch (and a glance into 2606.xxxxx and late-2605) so cross-boundary papers are not dropped. Target was ~5 entries; expanded to 10 primary entries because **MLSys 2026 ran during the window** (May 18–22, Bellevue) — invited talks, posters, and vendor blog launches timed to the conference all landed together, alongside a substantive Cohere model release with explicit KV-cache engineering and three new in-window KV-systems arXiv papers.

**Scope:** distributed caching, KV cache, caching for inference, storage-system caching.

**Selection criteria:** novelty of mechanism, potential systems / production impact, and whether the work measured runtime efficiency on a realistic workload — flagged explicitly even when the answer is "no" or "partial."

**Organization:** (A) production measurements / empirical studies — real deployments, real traces, real hardware, vendor-shipped releases — and (B) academic / idea-forward work — novel mechanisms typically evaluated on benchmarks or simulated traces.

**Methodology caveat:** arxiv.org, mlsys.org, and several vendor pages (vllm.ai/blog, blog.vllm.ai, developers.redhat.com) returned HTTP 403 to direct WebFetch this week; arXiv abstracts, the MLSys virtual program, and the vLLM blog post body were reconstructed from search-index snippets and cross-checked against multiple references where possible. Treat experimental figures attributed to arXiv preprints and vendor blogs as "as-claimed" pending a PDF or full-page read. Several papers carry a v1 date inside the window but were not surfaced by the prior week's sweep and are first-included here.

---

## A. Production measurements and empirical studies

### 1. vLLM × Novita AI — PegaFlow: Production-Grade External KV Cache (May 18, 2026)

- Reference: [vLLM Blog — vLLM x Novita AI: PegaFlow for Production-Grade External KV Cache (May 18, 2026)](https://blog.vllm.ai/2026/05/18/pegaflow-novita.html). Companion: [novitalabs/pegaflow on GitHub](https://github.com/novitalabs/pegaflow).
- Summary: PegaFlow is a **standalone Rust process** that integrates with vLLM (and SGLang) via the external KV-cache connector interface. It offloads KV cache to host DRAM or SSD and shares it across nodes over RDMA, running as a sidecar so the cache survives engine restarts and scales independently of the inference replica set. The design rationale is squarely "GIL-free, zero Python overhead on the hot path, Prometheus + OTLP observability, drop-in connector" — i.e., production hygiene around a tiered/distributed KV store. It is the first first-party vLLM-blog feature of an *external* KV-cache implementation that is not part of LMCache or Mooncake.
- Novelty: Medium-high. The "external KV cache as a sidecar Rust process with its own lifecycle" framing is distinct from LMCache (Python-embedded) and Mooncake (cross-instance store). It crystallizes the pattern the v0.21.0 HMA/Mooncake-connector work made possible: the KV tier is now a separate, language-of-choice service.
- Impact: High for operators standing up tiered KV in production — independent restart/scaling and RDMA-cross-node sharing without taking on LMCache's deployment shape were both common asks.
- Runtime evaluation: Partial. The blog post emphasizes architecture and integration; specific TTFT/throughput/hit-rate numbers were not surfaced in the snippets we could read. Flagged as a gap — this is exactly the kind of release that needs a follow-on benchmark post.

### 2. Databricks — Accelerating LLM Inference with Prompt Caching for Open-Source Models on Databricks (May 22, 2026)

- Reference: [Databricks Blog — Accelerating LLM Inference with Prompt Caching for Open-Source Models on Databricks (May 22, 2026)](https://www.databricks.com/blog/accelerating-llm-inference-prompt-caching-open-source-models-databricks).
- Summary: Databricks now supports prompt-prefix caching for open-source models across batch, pay-per-token, and provisioned-throughput workloads with no setup required from the user. Reports **2.5× throughput and 3× P50 latency reduction on a production GPT-OSS workload** (as-claimed). Pairs naturally with the May 8 Superhuman / Databricks 200K-QPS write-up — the same platform now exposes prefix caching as a uniform feature across pricing tiers.
- Novelty: Low-medium as a mechanism (prefix caching is standard), but high as a delivery contribution: prompt caching as a managed, no-configuration feature across all three Databricks LLM serving SKUs is a real productization milestone.
- Impact: High for the Databricks Mosaic customer base; medium as a reference point. Confirms the industry direction that prompt/prefix caching is now table-stakes for managed LLM serving, not an opt-in optimization.
- Runtime evaluation: Yes (vendor-reported). 2.5×/3× numbers on a production GPT-OSS workload. No tail-latency distribution or hit-rate breakdown in the available text; treat as headline-only.

### 3. MLSys 2026 — LMCache invited talk and Kitty poster (May 18–22, 2026, Bellevue)

- Reference: [MLSys 2026 — Invited Talk: LMCache, An Efficient KV Cache Layer for Enterprise-Scale LLM Inference (Mon 2026-05-18)](https://mlsys.org/virtual/2026/invited-talk/3646). Companions: [MLSys 2026 — Kitty poster (arXiv:2511.18643)](https://mlsys.org/virtual/2026/poster/3523), [MLSys 2026 papers list](https://mlsys.org/virtual/2026/papers.html), [LMCache tech report PDF](https://lmcache.ai/tech_report.pdf).
- Summary: MLSys 2026 opened on the first day of the window. The **LMCache invited talk** (Yuhan Liu, Grand Ballroom 1, Monday 9:50 PDT) consolidates LMCache's now-canonical pitch: pull KV cache out of GPU memory, share it across vLLM/SGLang engines and queries, support cache offloading (prefix reuse) and prefill-decode disaggregation, **up to 15× throughput on multi-round QA / document-analysis workloads** (as-claimed). The **Kitty poster** lands the algorithm-system co-design for 2-bit KV the prior weekly previewed: channel-sensitivity-ranked precision boost on top of 2-bit, page-centric KV layout with Triton dequant kernels, claiming ~8× KV-memory reduction, 8× larger batches, and 2.1–4.1× throughput at fixed memory budget across Qwen3/LLaMA-3. The conference also presented MorphServe and FlexiCache (both previewed last week) and ContextPilot (4–12× cache hits, 1.5–3× faster prefill across vLLM/SGLang/llama.cpp).
- Novelty: Medium-high. The talk consolidates rather than introduces; the Kitty poster is a clean instantiation of channel-adaptive 2-bit KV with a measured systems angle.
- Impact: High. MLSys 2026 is the in-window venue event, and LMCache is the de facto reference KV-cache layer for vLLM/SGLang in 2026 production deployments. The same week SemiAnalysis previewed picks publicly and SGLang/Ai2/Crusoe ran community gatherings around the conference — the KV-cache stack now has a centralized community moment, not just papers in isolation.
- Runtime evaluation: Yes — strong for LMCache (15× throughput on the cited workloads) and Kitty (throughput at fixed memory budget across two model families); partial for the broader program, which we covered last week.

### 4. Cohere — Command A+ launch with explicit KV-cache footprint engineering (May 20, 2026)

- Reference: [Cohere Blog — Introducing Command A+ (May 20, 2026)](https://cohere.com/blog/command-a-plus).
- Summary: Cohere's new frontier release (218B sparse MoE, 25B active, Apache 2.0, 2× H100 or 1× B200 deployable) is structurally relevant to the caching beat for two reasons. (1) The model **interleaves sliding-window attention layers (with RoPE) and global-attention layers (no PE) at a 3:1 ratio**, an architectural choice whose primary effect is to cut KV-cache size for long-context inference. (2) The Quantization-Aware Distillation recipe **keeps the attention path / KV cache at full precision** while quantizing MoE experts to W4A4 — the model team explicitly chose to spend their precision budget on the KV path rather than on weights. This is the same pattern xAI used in Grok 4.3 and DeepSeek used in V4; it is becoming the consensus shape.
- Novelty: Medium. The 3:1 hybrid-attention ratio isn't unique (Grok 4.3 reported 6:1, Kimi Linear / FlashKDA reported a 75% KV-cache reduction via KDA), but it is the cleanest documented case in the window of a frontier lab explicitly framing KV-cache footprint as a model-architecture decision rather than a serving-layer afterthought.
- Impact: High. Frontier model releases that bake KV-cache footprint into the architecture set the constraints inference vendors have to plan around. Command A+ now joins Grok 4.3 / Kimi K2.6 / DeepSeek V4 in the "the KV cache shape is decided at pretraining" cluster.
- Runtime evaluation: Partial. The blog reports model-quality benchmarks and deployment footprint; serving-throughput numbers on the hybrid attention ratio specifically are not broken out. The implication ("we can serve longer contexts on the same hardware because of the attention mix") is offered architecturally, not measured.

---

## B. Academic / idea-forward work

### 5. KVDrive — A Holistic Multi-Tier KV Cache Management System for Long-Context LLM Inference ([arXiv:2605.18071](https://arxiv.org/abs/2605.18071); v1 May 18, in window)

- Reference: [arXiv:2605.18071](https://arxiv.org/abs/2605.18071). SIGMOD 2026 accepted (per the accepted-paper list).
- Summary: A three-pillar multi-tier KV manager spanning GPU HBM, host DRAM, and NVMe SSD: (1) attention-behavior-aware placement to maximize reuse across tiers, (2) a restructured decoding pipeline that overlaps I/O with CPU/GPU compute stages, (3) cross-tier data-movement harmonization to coalesce transfers. The framing is explicitly that prior offloading systems hit an I/O wall once context and batch grow together, and that placement + pipelining + harmonization must be co-designed.
- Novelty: High. KVDrive is the cleanest in-window competitor to Tutti (entry #5 last week, GPU-direct SSD object store) and a direct alternative to LMCache's host-DRAM tier. The "attention-behavior-aware placement" framing is a sharper articulation of *what* to keep where than the prior recency-based heuristics.
- Impact: High. SIGMOD-accepted, on-beat for the storage-systems angle the field is converging toward, and pairs with the vLLM HMA + PegaFlow consolidation on the production side.
- Runtime evaluation: Yes (claimed). The paper is positioned as a systems contribution against existing offloading baselines; specific TTFT/TBT/throughput numbers were not visible in the available text but the framing is end-to-end serving runtime. Flagged for a deeper PDF read once arXiv access is restored.

### 6. KVServe — Service-Aware KV Cache Compression for Communication-Efficient Disaggregated LLM Serving ([arXiv:2605.13734](https://arxiv.org/abs/2605.13734); v1 May 13, boundary, not in any prior report)

- Reference: [arXiv:2605.13734](https://arxiv.org/abs/2605.13734).
- Summary: The first **service-aware adaptive** KV-compression framework for PD-disaggregated serving. Unifies KV-compression knobs into a composable strategy space; a Bayesian Profiling Engine searches that space online while sensing workload mix, bandwidth, and SLO/quality, and selects a per-request-period optimal profile (~50× lower offline search overhead, as-claimed). The framing is that KV compression in disaggregated serving is fundamentally a *control-plane* problem (which compression for which request, given current network and SLO state), not a model-side property.
- Novelty: High. The reframing of KV compression as a service-aware control knob — composable, profiled online, SLO/bandwidth-aware — is a genuinely new abstraction. Prior work mostly picks one compression scheme and ships it.
- Impact: Strong for any operator running prefill-decode disaggregation across machines (vLLM disagg, Together CPD, NVIDIA Dynamo). The natural pairing with PegaFlow and KVDrive is obvious: the data plane gets a tier, the control plane gets a profile.
- Runtime evaluation: Yes (claimed). End-to-end TTFT/TBT under varying SLOs on disaggregated serving stacks. Boundary v1 date (May 13) but absent from all prior reports — included as in-scope-new.

### 7. PEEK — Context Map as an Orientation Cache for Long-Context LLM Agents ([arXiv:2605.19932](https://arxiv.org/abs/2605.19932); v1 May 19, in window)

- Reference: [arXiv:2605.19932](https://arxiv.org/abs/2605.19932).
- Summary: PEEK treats *orientation knowledge* about a recurring external context (a document corpus, a code repository, a database schema) — what's in it, how it's organized, key entities/schemas — as a small, constant-sized "context map" persisted in the agent's prompt. A **programmable cache policy** maintains, invalidates, and refreshes the map across invocations. Frames agent state as a cache-replacement problem, not a retrieval/RAG problem.
- Novelty: High. Explicitly importing classical cache-replacement vocabulary (policy, invalidation, refresh, eviction) into agent-state design is fresh. It is the conceptual complement to PBKV (entry #6 last week, *predict* future-step reuse) — PEEK *summarizes* recurring contexts; PBKV predicts dynamic-workflow reuse.
- Impact: Potentially high if the abstraction sticks. The "agent prompt = small managed cache of orientation knowledge" idea generalizes well to coding agents, document-analysis agents, and long-running planners — all dominant commercial LLM workloads.
- Runtime evaluation: Partial. Agent-task benchmarks with cost/latency framing; not a serving-runtime evaluation against vLLM/SGLang. The contribution is abstraction-level. Flagged as the expected gap, not an exclusion.

### 8. GEM — GPU-Variability-Aware Expert-to-GPU Mapping for MoE Systems ([arXiv:2605.19945](https://arxiv.org/abs/2605.19945); v1 May 19, in window)

- Reference: [arXiv:2605.19945](https://arxiv.org/abs/2605.19945).
- Summary: Maps experts to GPUs while accounting for *per-GPU performance variability*. Argues that prior MoE placement work balances token *load* but ignores GPU heterogeneity, so the straggler GPU ends up bound to the hottest expert and dominates MoE-layer latency. GEM treats placement as a joint expert-affinity + GPU-capability assignment problem.
- Novelty: Medium-high. The straggler-aware framing is fresh in the MoE-serving literature, which has overwhelmingly focused on load balance and routing rather than per-GPU heterogeneity. Generalizes naturally to mixed-GPU clusters (H100s alongside H200s/B200s/MI300X).
- Impact: High for operators of mixed-GPU clusters — a common reality, not the edge case the literature treats it as. The KV-cache angle is indirect (MoE-layer latency, not KV placement) but real: GPU variability also shows up in KV-tier hit rates and is under-explored there.
- Runtime evaluation: Yes (claimed). Real GPU-cluster measurements of straggler and throughput effects; the framing demands it. Specific numbers not visible in available text.

### 9. OScaR — Omni-Scaled Canalized Rotation for Extreme KV Cache Quantization ([arXiv:2605.19660](https://arxiv.org/abs/2605.19660); v1 May 19, in window)

- Reference: [arXiv:2605.19660](https://arxiv.org/abs/2605.19660). Code released.
- Summary: Identifies "Token Norm Imbalance" (TNI) as the dominant accuracy-loss source under extreme per-channel KV quantization, then proposes OScaR — a lightweight rotation-based scheme — for extreme compression on both text-only and multimodal LLMs. Sits in the KIVI / Polar / QuaRot rotation-quantization lineage but extends explicitly to multimodal KV.
- Novelty: Medium. Rotation-based extreme KV quantization is an active sub-field; the multimodal angle and the TNI characterization are the fresh contributions.
- Impact: Medium-high. Multimodal serving (vision-language, audio) is where the KV/context-cache footprint problem is genuinely worse, and most KV-quant work to date is text-only.
- Runtime evaluation: Partial. The paper is pitched as a Pareto-front (accuracy × compression) result; runtime/throughput numbers are unclear from the abstract.

### 10. Protection Is (Nearly) All You Need — Structural Protection Dominates Scoring in Globally Capped KV Eviction ([arXiv:2605.18053](https://arxiv.org/abs/2605.18053); v1 May 18, in window)

- Reference: [arXiv:2605.18053](https://arxiv.org/abs/2605.18053).
- Summary: A measurement / comparison paper that puts **seven** eviction policies (LRU, H2O, SnapKV, StreamingLLM, Ada-KV, QUEST, Random) under a shared decode-time cap and asks a uniform question: how much of the gap between policies is from *which tokens get scored* versus *which tokens get structurally protected*. The headline: structural protection of key tokens dominates scoring across all seven policies, and the field has been overfitting to scoring heuristics. Also surfaces a shared prompt-boundary vulnerability across policies.
- Novelty: High as a measurement contribution. Apples-to-apples eviction-policy bake-offs are rare; the structural-vs-scoring decomposition is exactly the methodological hygiene the field needs after a year of competing eviction papers.
- Impact: High for anyone designing or operating an eviction policy. The implication — start from a strong structural-protection baseline and only then bolt on scoring — directly affects how the next round of policies should be built.
- Runtime evaluation: Partial. Accuracy-under-cap is the headline measurement; whether serving-throughput / TTFT was also measured was not visible in the available text. Still in the empirical-hygiene track that the prior weekly identified as a theme.

---

## Additional context (noted this week, not selected as top entries)

- **Storage-system caching, in/near window:**
  - [Google Cloud — Cloud Storage Rapid turbocharges object storage for AI, analytics (May 12, 2026)](https://cloud.google.com/blog/products/storage-data-transfer/cloud-storage-rapid-turbocharges-object-storage-for-ai-analytics): Rapid Bucket + Rapid Cache (formerly Anywhere Cache); "ingest on write" eliminates first-read cache miss; vendor-reported up to 2.2× faster checkpoint restore; Rapid Cache now serves up to 20% of Cloud Storage global egress with 20× YoY caches-deployed growth. Boundary date.
  - [MinIO — Introducing MinIO MemKV (May 12, 2026)](https://www.min.io/blog/introducing-minio-memkv) and [Blocks & Files coverage (May 12, 2026)](https://www.blocksandfiles.com/ai-ml/2026/05/12/minio-adds-petabyte-scale-memkv-cache-for-nvidia-gpu-inference/): petabyte-scale DRAM/NVMe context-memory store for Nvidia GPU inference clusters, RDMA-accessible KV/context tier on top of AIStor, compliant with Nvidia STX. Vendor-reported microsecond-scale access. Boundary date.
- **Inference-stack adjacent reads, in window:**
  - [Red Hat Developer — What GPU kernels mean for your distributed inference (May 20, 2026)](https://developers.redhat.com/articles/2026/05/20/what-gpu-kernels-mean-your-distributed-inference): unversioned `get_kernel()` calls as silent external dependencies across replicas. Tangential to KV caching, relevant to distributed inference hygiene.
  - [Red Hat Developer — How to prevent silent failures in your production AI inference stack (May 22, 2026)](https://developers.redhat.com/articles/2026/05/22): TTFT-threshold-based Day-2 ops automation; Kubernetes liveness/readiness probes miss inference degradation. Tangential.
  - [Snowflake — Batch Inference at Scale with SPCS and Ray (May 20, 2026)](https://www.snowflake.com/en/blog/engineering/snowflake-batch-inference-jobs-spcs/): batch-inference architecture via AI_COMPLETE / Snowflake ML in SQL. No caching-specific detail.
- **arXiv in-window, weaker fit:**
  - [arXiv:2605.15051 — An Interpretable Latency Model for Speculative Decoding in LLM Serving](https://arxiv.org/abs/2605.15051) (v1 May 14, boundary, MIT + Red Hat AI): closed-form latency model under continuous batching, Little's-Law-derived effective batch size, validated with extensive vLLM measurements across drafter/target pairs and request rates. **Strong serving-realism eval**; included here because the topic is spec-decode rather than caching directly. Operators tuning spec-decoding under SLOs should read it.
  - [arXiv:2605.16928 — Full Attention Strikes Back: Transferring Full Attention into Sparse within Hundred Training Steps](https://arxiv.org/abs/2605.16928) (v1 May 16, boundary): converts full-attention LLMs to sparse in ~100 training steps using three signals (only a head subset needs full long-context; retrieval lives in a 16-dim subspace; useful budget is query-dependent). Training-cost-led, not a caching paper per se but directly affects KV-cache footprint of the converted model.
  - [arXiv:2605.17304 — Compress the Context, Keep the Commitments (Context Codec)](https://arxiv.org/abs/2605.17304) (v1 May 17): typed semantic atoms with conflict/confidence/risk fields for verifiable LLM context compression. Semantic-cache analog to prompt compression; IR-style eval, no serving runtime.
- **Just past window (surface next week):**
  - [arXiv:2605.20179 — TIDE: Efficient and Lossless MoE Diffusion LLM Inference with I/O-aware Expert Offload](https://arxiv.org/abs/2605.20179) (v1 May 26): exploits temporal stability of expert activations across diffusion denoising steps to cache experts and avoid reload thrashing on memory-limited devices.
  - [arXiv:2605.20813 — PulseCol: Periodically Refreshed Column-Sparse Attention for Diffusion LLMs](https://arxiv.org/abs/2605.20813) (v1 May 27): column-sparse (vs. block-sparse) attention with periodic refresh; up to 1.95× over FlashAttention at 64K context.
  - [arXiv:2605.20868 — Runtime-Certified Bounded-Error Quantized Attention](https://arxiv.org/abs/2605.20868) (v1 May 27): tiered KV (INT8 keys + INT4 values in HBM, FP16 originals in CPU RAM as deterministic fallback) with online error bounds driving adaptive precision; PG-19/NIAH/RULER on LLaMA-3.1-8B up to 128K. The *certified*-fallback ladder is unusual.
- **Curated trackers and discussion:** [TreeAI-Lab/Awesome-KV-Cache-Management](https://github.com/TreeAI-Lab/Awesome-KV-Cache-Management), [jjiantong/Awesome-KV-Cache-Optimization](https://github.com/jjiantong/Awesome-KV-Cache-Optimization), [October2001/Awesome-KV-Cache-Compression](https://github.com/October2001/Awesome-KV-Cache-Compression) all added MLSys-2026 entries during the week. Hacker News: no marquee cache-specific front-page thread in window; pre-window discussions on TurboQuant and Modular's "Five Eras of KVCache" continued to draw traffic. SemiAnalysis [MLSys 2026 preview thread on X](https://x.com/SemiAnalysis_/status/2055845794020757678) and [LMSYS MLSys Happy Hour announcement](https://x.com/lmsysorg/status/2054008108205437413) framed the conference week.
- **Explicit negatives this week:**
  - No new in-window CacheLib / CacheLib-FDP / RocksDB / Ceph / MinIO (other than MemKV at boundary) / AWS storage blog post.
  - No new in-window LMCache / SGLang / Mooncake / NVIDIA Dynamo / TensorRT-LLM / llm-d / KServe / Together AI / Fireworks / Anyscale / Modal / Replicate / RunPod / CoreWeave / VAST / WEKA / Pure Storage post.
  - No new in-window Anthropic / OpenAI / Google DeepMind / Meta FAIR / xAI / Mistral / DeepSeek / Moonshot-Kimi / Qwen-Alibaba / Zhipu-GLM / 01.AI / Apple / Amazon Science / Microsoft Research / Tencent AI Lab / ByteDance Seed / Baidu Research engineering blog (Cohere Command A+ is the only frontier-lab release with KV-cache implications in window).
  - No in-window content from OSDI / ATC / USENIX Security / SOSP / SIGCOMM / EuroSys / ASPLOS / VLDB / HotOS / HotNets / HotStorage / SoCC 2026 — accepted lists either pre-public or pre-window. SIGMOD 2026 list is public and includes KVDrive (entry #5). MLSys 2026 is the in-window venue event (entry #3).
  - No in-window non-LLM distributed/storage-system caching paper (CDN/object-cache) beyond the MemKV / Storage Rapid vendor posts.

---

## Cross-cutting observations (this week)

- **MLSys 2026 makes the KV-cache stack a community event.** The week's center of gravity is the conference itself: an LMCache invited talk, a Kitty poster, MorphServe / FlexiCache / ContextPilot presentations, SemiAnalysis previews, and SGLang/LMSYS gatherings all in Bellevue at the same moment. The prior weekly identified the program; this weekly observes that the *community* — vendors, paper authors, and operators — coalesced around it. Expect MLSys-week presentations to seed a wave of follow-on blog posts and integrations through June.
- **External KV cache as a separate process is now a pattern, not a one-off.** PegaFlow (entry #1, Rust sidecar) joins LMCache (Python in-process) and Mooncake (cross-instance store) as the third major shape. The vLLM connector interface is doing the load-bearing work — the *cache implementation* has become a pluggable choice independent of the inference engine, with corresponding hygiene improvements (independent restart, RDMA cross-node sharing, language-of-choice). The implication for storage-systems people: this is now an external-storage-tier integration problem, not a Python optimization problem.
- **KV-cache footprint is moving into the pretraining decision.** Cohere Command A+ joins Grok 4.3, Kimi K2.6 / Linear, and DeepSeek V4 in a cluster of frontier releases where the attention mix is explicitly chosen to shrink KV cache. The signal: serving teams should expect future frontier models to ship with attention shapes co-designed against KV footprint, and inference stacks should plan around heterogeneous attention layers (sliding-window + global + linear) rather than the homogeneous all-full-attention assumption.
- **Eviction-policy methodology starts to catch up with the mechanism flood.** "Protection Is (Nearly) All You Need" (entry #10) is the methodological complement to last week's LMCache hashseed caveat and TurboQuant reality-check: the field is *finally* doing apples-to-apples bake-offs across the past year's eviction proposals and finding that the differences are smaller than the proposals' framing implied. The same hygiene shift is now visible in both empirical (vendor benchmarks) and academic (comparison papers) work.
- **The cache abstraction expands beyond KV.** PEEK (entry #7) frames agent orientation knowledge as a managed cache; Apple's Krites (boundary, prior month) frames semantic responses as a cache; Mooncake / PegaFlow / KVDrive / KVServe frame KV as a tiered store; Cohere/Grok/DeepSeek frame attention as a footprint-budgeting problem. "Cache" is becoming the load-bearing abstraction across four distinct layers of the LLM serving stack — model architecture, attention runtime, KV memory tier, and agent state.

## References

- [vLLM Blog — vLLM x Novita AI: PegaFlow for Production-Grade External KV Cache (May 18, 2026)](https://blog.vllm.ai/2026/05/18/pegaflow-novita.html) · [novitalabs/pegaflow on GitHub](https://github.com/novitalabs/pegaflow) · [vLLM v0.21.0 release notes](https://github.com/vllm-project/vllm/releases/tag/v0.21.0)
- [Databricks Blog — Accelerating LLM Inference with Prompt Caching for Open-Source Models on Databricks (May 22, 2026)](https://www.databricks.com/blog/accelerating-llm-inference-prompt-caching-open-source-models-databricks) · [Databricks — Superhuman 200K-QPS platform (May 8, 2026)](https://www.databricks.com/blog/how-superhuman-and-databricks-built-200k-qps-inference-platform-together)
- [MLSys 2026 — LMCache invited talk](https://mlsys.org/virtual/2026/invited-talk/3646) · [MLSys 2026 — Kitty poster (arXiv:2511.18643)](https://mlsys.org/virtual/2026/poster/3523) · [MLSys 2026 papers page](https://mlsys.org/virtual/2026/papers.html) · [LMCache tech report](https://lmcache.ai/tech_report.pdf) · [LMCache arXiv:2510.09665](https://arxiv.org/abs/2510.09665) · [Kitty arXiv:2511.18643](https://arxiv.org/abs/2511.18643)
- [Cohere Blog — Introducing Command A+ (May 20, 2026)](https://cohere.com/blog/command-a-plus)
- [arXiv:2605.18071 — KVDrive: A Holistic Multi-Tier KV Cache Management System](https://arxiv.org/abs/2605.18071) · [SIGMOD 2026 accepted papers](https://2026.sigmod.org/sigmod_papers.shtml)
- [arXiv:2605.13734 — KVServe: Service-Aware KV Cache Compression for Disaggregated LLM Serving](https://arxiv.org/abs/2605.13734)
- [arXiv:2605.19932 — PEEK: Context Map as an Orientation Cache for Long-Context LLM Agents](https://arxiv.org/abs/2605.19932)
- [arXiv:2605.19945 — GEM: GPU-Variability-Aware Expert-to-GPU Mapping for MoE Systems](https://arxiv.org/abs/2605.19945)
- [arXiv:2605.19660 — OScaR: Omni-Scaled Canalized Rotation for Extreme KV Cache Quantization](https://arxiv.org/abs/2605.19660)
- [arXiv:2605.18053 — Protection Is (Nearly) All You Need: Structural Protection Dominates Scoring in Globally Capped KV Eviction](https://arxiv.org/abs/2605.18053)
- [Google Cloud — Cloud Storage Rapid turbocharges object storage for AI, analytics (May 12, 2026)](https://cloud.google.com/blog/products/storage-data-transfer/cloud-storage-rapid-turbocharges-object-storage-for-ai-analytics)
- [MinIO — Introducing MinIO MemKV (May 12, 2026)](https://www.min.io/blog/introducing-minio-memkv) · [Blocks & Files — MinIO MemKV coverage (May 12, 2026)](https://www.blocksandfiles.com/ai-ml/2026/05/12/minio-adds-petabyte-scale-memkv-cache-for-nvidia-gpu-inference/)
- [Red Hat Developer — What GPU kernels mean for your distributed inference (May 20, 2026)](https://developers.redhat.com/articles/2026/05/20/what-gpu-kernels-mean-your-distributed-inference) · [Red Hat Developer — How to prevent silent failures in your production AI inference stack (May 22, 2026)](https://developers.redhat.com/articles/2026/05/22)
- [Snowflake — Batch Inference at Scale with SPCS and Ray (May 20, 2026)](https://www.snowflake.com/en/blog/engineering/snowflake-batch-inference-jobs-spcs/)
- [arXiv:2605.15051 — Interpretable Latency Model for Speculative Decoding in LLM Serving](https://arxiv.org/abs/2605.15051)
- [arXiv:2605.16928 — Full Attention Strikes Back: Transferring Full Attention into Sparse within Hundred Training Steps](https://arxiv.org/abs/2605.16928)
- [arXiv:2605.17304 — Compress the Context, Keep the Commitments (Context Codec)](https://arxiv.org/abs/2605.17304)
- [arXiv:2605.20179 — TIDE: MoE Diffusion LLM Inference with I/O-aware Expert Offload](https://arxiv.org/abs/2605.20179) · [arXiv:2605.20813 — PulseCol: Column-Sparse Attention for Diffusion LLMs](https://arxiv.org/abs/2605.20813) · [arXiv:2605.20868 — Runtime-Certified Bounded-Error Quantized Attention](https://arxiv.org/abs/2605.20868)
- [TreeAI-Lab/Awesome-KV-Cache-Management](https://github.com/TreeAI-Lab/Awesome-KV-Cache-Management) · [jjiantong/Awesome-KV-Cache-Optimization](https://github.com/jjiantong/Awesome-KV-Cache-Optimization) · [October2001/Awesome-KV-Cache-Compression](https://github.com/October2001/Awesome-KV-Cache-Compression)
- [SemiAnalysis MLSys 2026 preview thread (X)](https://x.com/SemiAnalysis_/status/2055845794020757678) · [LMSYS MLSys 2026 Happy Hour announcement (X)](https://x.com/lmsysorg/status/2054008108205437413) · [Modular — The Five Eras of KVCache](https://www.modular.com/blog/the-five-eras-of-kvcache)
