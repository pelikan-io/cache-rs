# Weekly Cache Research Report — 2026-05-17

**Run type:** Normal weekly run. Prior report: [`weekly-cache-report-2026-05-10.md`](./weekly-cache-report-2026-05-10.md). Search horizon: 2026-05-11 → 2026-05-17 (the seven days following the prior report), with a deliberate sweep of the 2605.xxxxx arXiv batch (and a glance into 2606) so cross-boundary papers are not dropped. Target was ~5 entries; expanded to 8 primary entries because a vLLM TurboQuant reality-check, a vLLM v0.21.0 release, an AMD-hardware LMCache benchmark, the MLSys 2026 program going live in-window, and a cluster of arXiv KV-serving systems papers all landed together.

**Scope:** distributed caching, KV cache, caching for inference, storage-system caching.

**Selection criteria:** novelty of mechanism, potential systems / production impact, and whether the work measured runtime efficiency on a realistic workload — flagged explicitly even when the answer is "no" or "partial."

**Organization:** (A) production measurements / empirical studies — real deployments, real traces, real hardware — and (B) academic / idea-forward work — novel mechanisms typically evaluated on benchmarks or simulated traces.

**Methodology caveat:** arxiv.org, usenix.org, mlsys.org and several vendor pages returned HTTP 403 to direct fetches this week; arXiv and conference details below were reconstructed from search-index snippets of the abstract/HTML pages and cross-checked against dblp where possible. Treat experimental figures attributed to arXiv preprints as "as-claimed" pending a PDF read. Several quantization/eviction papers carry an April v1 date but first appear in the May listing batch and were absent from all prior reports — these are flagged inline.

---

## A. Production measurements and empirical studies

### 1. vLLM — "A First Comprehensive Study of TurboQuant: Accuracy and Performance" (May 11, 2026)

- Reference: [vLLM Blog — A First Comprehensive Study of TurboQuant: Accuracy and Performance (May 11, 2026)](https://vllm.ai/blog/2026-05-11-turboquant). Companions: [vLLM v0.21.0 release notes](https://github.com/vllm-project/vllm/releases/tag/v0.21.0), prior-week [vLLM v0.20.0 release (TurboQuant 2-bit KV default)](https://github.com/vllm-project/vllm/releases/tag/v0.20.0).
- Summary: The first rigorous, vendor-neutral accuracy-vs-performance study of TurboQuant low-bit KV-cache quantization, run by the vLLM team across four dense/MoE models (30B–200B+) and five benchmarks chosen to stress both prefill-heavy long-context retrieval (`openai/mrcr`) and decode-heavy reasoning (AIME25, GPQA:Diamond, MATH500, LiveCodeBench-v6). The headline conclusion cuts against the TurboQuant hype cycle: **FP8 (`--kv-cache-dtype fp8`) remains the best default** — ~2× KV capacity, negligible accuracy loss, BF16-matching performance — while TurboQuant `k8v4` yields only ~2.4× capacity at a throughput/latency penalty not worth paying, and `4bit-nc` is the only practical TurboQuant variant, useful strictly under memory pressure. Dequantization back to BF16 before attention scales with KV volume, which is where the overhead comes from.
- Novelty: High as a measurement contribution. The mechanism (TurboQuant) is not new — it shipped as a vLLM default in v0.20.0 last fortnight and traces to the Google ICLR 2026 paper — but a sober first-party "the default you just shipped is usually worse than FP8" study is exactly the kind of empirical correction the field rarely publishes about its own releases.
- Impact: High and immediately actionable. This is the reference operators will cite when deciding KV dtype for the v0.20–v0.21 line; it reframes the prior-week "TurboQuant 2-bit by default" headline as "FP8 unless you are genuinely memory-bound."
- Runtime evaluation: Yes — strong. Per-variant latency overheads on real models (Qwen3-30B ~10–60%; Llama-3.3-70B ~10–68%), capacity multipliers (2× FP8 vs 2.4× `k8v4`), and accuracy across five benchmarks. Tail-latency distributions under continuous batching are the obvious next cut.

### 2. LMCache — "Benchmarking LMCache for Multi-Turn Agentic Workloads on AMD MI300X" (May 12, 2026)

- Reference: [LMCache Blog — Benchmarking LMCache for Multi-Turn Agentic Workloads on AMD MI300X (May 12, 2026)](https://blog.lmcache.ai/en/2026/05/12/benchmarking-lmcache-for-multi-turn-agentic-workloads-on-amd-mi300x/). Companions: prior-week [LMCache — DeepSeek V4 and your wallet (May 4, 2026)](https://blog.lmcache.ai/en/2026/05/04/deepseek-v4-explained-and-why-it-matters-to-your-wallet/), [LMCache on Amazon SageMaker HyperPod (Apr 22, 2026)](https://blog.lmcache.ai/en/2026/04/22/lmcache-on-amazon-sagemaker-hyperpod-accelerating-llm-inference-with-managed-tiered-kv-cache/).
- Summary: LMCache benchmarked on 2× AMD MI300X with vLLM 0.19.0 serving MiniMax-M2.5 (230 GB FP8 MoE), driven by 739 anonymized Claude Code conversation traces. Under stress (32 users / 100K context / agentic traces) LMCache delivered **3.0× lower average TTFT, 2.1× lower p95, 2.6× lower max, and 2.3× more completed requests** versus HBM-only. The operational findings are the real contribution: `PYTHONHASHSEED=0` is mandatory or cache keys diverge and hit rate collapses to 0% even on bit-identical prompts; synthetic fixed-hit-rate benchmarks make LMCache look 10–17% *worse* because they never reproduce the memory-pressure regime where it helps; and at low load the HBM prefix cache wins (5.8× more requests) because the working set fits L1.
- Novelty: Medium-high. Distinct from the prior-week vLLM × Mooncake agentic case study — different hardware (MI300X/ROCm), different model (MiniMax-M2.5), and a methodological critique of synthetic cache-hit benchmarking that generalizes well beyond LMCache.
- Impact: High for AMD/ROCm operators and for anyone benchmarking a KV-cache tier. The "only wins under genuine memory pressure" and hashseed caveats are the kind of thing that silently invalidates a quarter of cache A/B tests.
- Runtime evaluation: Yes — real agentic traces, TTFT (avg/p95/max), request-completion multipliers, and cache-key-consistency caveats, plus the explicit load-regime crossover where HBM-only is the better choice.

### 3. vLLM v0.21.0 — HMA-integrated KV offloading, Mooncake connector, NVFP4 KV (May 15, 2026)

- Reference: [vLLM v0.21.0 release notes (May 15, 2026)](https://github.com/vllm-project/vllm/releases/tag/v0.21.0). Companions: prior-week [v0.20.2](https://github.com/vllm-project/vllm/releases/tag/v0.20.2) and [v0.20.1](https://github.com/vllm-project/vllm/releases/tag/v0.20.1).
- Summary: The first release after the v0.20.x DeepSeek-V4 stabilization line, and a substantive consolidation of vLLM's tiered/distributed KV stack. It integrates KV offloading with the **Hybrid Memory Allocator (HMA)**: scheduler-side sliding-window groups (#41228), full HMA enablement (#41445), multi-connector HMA (#39571), per-job store completion (#39186), DCP/PCP support in `OffloadingConnector` (#41549), and a new **`MooncakeStoreConnector`** for distributed KV offloading (#40900). Also ships TurboQuant 2-bit KV (4× capacity) and NVFP4 KV cache support, a FlashInfer top-k/top-p sampler default, and ~51% faster `AllPool.forward`.
- Novelty: Medium. Incremental but real — this is the release where vLLM's distributed/tiered KV offloading (HMA + Mooncake connector + low-bit KV) becomes one coherent subsystem rather than a set of connectors, closing the loop on the prior-week vLLM × Mooncake recommendation.
- Impact: High. The HMA-plus-offloading integration and an upstream Mooncake store connector are core production infrastructure; this is the version operators will build tiered-KV deployments on.
- Runtime evaluation: Partial. Only the `AllPool.forward` ~51% speedup is quantified in the notes; no end-to-end cache-hit/TTFT/throughput numbers for the offloading or low-bit-KV features. Entry #1 (TurboQuant study) supplies the rigorous numbers for the 2-bit KV feature shipped here.

### 4. MLSys 2026 program live in-window — FlexiCache, MorphServe, Kitty (conference opens May 17, 2026)

- Reference: [MLSys 2026 papers page](https://mlsys.org/virtual/2026/papers.html). Primary papers: FlexiCache (MLSys '26), [MorphServe (arXiv:2506.02006)](https://arxiv.org/abs/2506.02006), Kitty (MLSys '26).
- Summary: MLSys 2026 (Bellevue, May 17–22) opens on the last day of the window, so its program is the in-window venue event. Three standout cache papers: **FlexiCache** classifies attention heads by *temporal stability* and keeps only unstable heads' KV fully resident while offloading stable ones, reporting up to 70% GPU-memory reduction for long context, 1.38–1.55× offline throughput, and 1.6–2.1× lower online token latency. **MorphServe** couples token-level asynchronous quantized layer swapping with pressure-aware KV-cache resizing under SLO pressure, reporting 92.45% fewer SLO violations and 2.2–3.9× better P95 TTFT versus full precision. **Kitty** does 2-bit KV quantization with a dynamic per-channel precision boost to recover accuracy at 2 bits.
- Novelty: Medium-high. FlexiCache's head-level temporal-stability signal driving a hierarchical GPU/host cache is the freshest of the three; MorphServe's joint layer-precision-swap + KV-resize control loop is a clean articulation of SLO-pressure-aware memory management; Kitty is incremental within the crowded 2-bit-KV track but channel-adaptive.
- Impact: Medium-high. These are peer-reviewed, serving-measured systems papers landing exactly as the production stack (vLLM v0.21.0) consolidates tiered/low-bit KV — FlexiCache and MorphServe in particular map directly onto problems vLLM/SGLang operators have today.
- Runtime evaluation: Yes for FlexiCache (offline + online long-context serving) and MorphServe (SLO/latency under serving load); partial for Kitty (accuracy-led; runtime claims thinner in available text).

---

## B. Academic / idea-forward work

### 5. Tutti — Making SSD-Backed KV Cache Practical for Long-Context LLM Serving ([arXiv:2605.03375](https://arxiv.org/abs/2605.03375); v1 ~May 5, boundary, not in any prior report)

- Reference: [arXiv:2605.03375](https://arxiv.org/abs/2605.03375).
- Summary: A GPU-centric KV-cache object store that removes the CPU from the critical data/control path between HBM and NVMe SSDs. Uses GPU-native object abstractions, a "GPU io_uring"-style asynchronous GPU-direct object I/O path, and slack-aware I/O scheduling to saturate SSD bandwidth and drive GPU stalls toward zero, targeting the three-tier (HBM–DRAM–SSD) prefix-cache offloading problem. Integrated into vLLM.
- Novelty: High. A genuine systems contribution — GPU-driven direct storage I/O for KV cache — distinct from the CPU-mediated offload path of LMCache and from the HMA connector model vLLM v0.21.0 ships. It is the cleanest articulation yet of "treat the SSD tier as a GPU-addressable object store, not a CPU-staged spill area."
- Impact: Strong. If SSD-backed KV can match DRAM-backed LMCache at near-infinite capacity, the economics of long-context and agentic serving change materially; pairs naturally with the vLLM HMA/Mooncake consolidation (entry #3) and the prior-week DDN/Lustre shared-cache pitch.
- Runtime evaluation: Yes. Implemented in vLLM; reports 78.3% TTFT reduction under a strict SLO and 2× higher achievable request rate versus the GDS-enabled SSD-backed LMCache baseline — a realistic serving-style evaluation against a strong SOTA baseline. Boundary date (v1 just before the window) but absent from all prior reports, so included as in-scope-new.

### 6. PBKV — Prediction-based KV-Cache Management for Dynamic Agent Workflows ([arXiv:2605.06472](https://arxiv.org/abs/2605.06472); v1 May 7, in window)

- Reference: [arXiv:2605.06472](https://arxiv.org/abs/2605.06472).
- Summary: Predicts future agent invocations by fusing historical-workflow guidance with the current task context, then converts multi-step forecasts into conservative eviction + prefetching decisions over a two-tier store (GPU radix tree + host HiCache). The target is *dynamic, data-dependent* agent sequences that workflow-static caches (e.g., KVFlow) miss because they assume a fixed workflow graph.
- Novelty: Medium-high. Predictive, dynamic-workflow-aware cache management is a fresher angle than the static workflow-graph schemes; it is the agentic-serving complement to the prior-week SAGA (workflow-atomic scheduling) — PBKV predicts reuse, SAGA schedules around it.
- Impact: High for agentic serving (multi-agent pipelines with shared but data-dependent context), now the dominant commercial vLLM workload.
- Runtime evaluation: Yes (partial). Up to 1.85× speedup over LRU on dynamic workflows and up to 1.26× over SOTA KVFlow on static workflows across three workflow benchmarks. Speedup measured; TTFT/TBT against vLLM/SGLang not broken out in available text.

### 7. Fluxion — Hybrid Sparse Attention with CPU-GPU Parallelism for Long-Context Inference ([arXiv:2605.07719](https://arxiv.org/abs/2605.07719); v1 ~May 12, in window)

- Reference: [arXiv:2605.07719](https://arxiv.org/abs/2605.07719).
- Summary: Sparse attention over CPU-resident KV caches built on three ideas — output-aware KV budgeting, head-specific/granularity-aware sparse configuration, and cross-device (CPU-GPU) coordinated execution. The argument is that block-sparsity alone is insufficient for end-to-end gains once the KV cache lives on the CPU, because the bottleneck shifts to CPU-GPU transfer scheduling.
- Novelty: Medium-high. Co-designing sparsity granularity with CPU-GPU pipelining for offloaded KV is a sharper framing than "apply sparse attention, then offload" and complements the SSD-tier work (Tutti, entry #5) on the DRAM-tier side.
- Impact: Medium. Relevant for single-GPU long-context serving with host-memory KV offload — a common cost-constrained deployment shape.
- Runtime evaluation: Yes. Across 2 models / 3 benchmarks / 40 tasks: 1.5–3.7× speedup over the strongest fixed sparse-hybrid baseline (KV budget 0.05), ~1.9× on Llama, GPU idle ratio cut from 70.65%→45.78% at 32K context on Qwen, <0.26 average quality degradation vs. full attention. Baseline is a sparse-hybrid method rather than vLLM/SGLang directly.

### 8. KV-RM — Regularizing KV-Cache Movement for Static-Graph LLM Serving ([arXiv:2605.09735](https://arxiv.org/abs/2605.09735); v1 ~May 10–13, in window)

- Reference: [arXiv:2605.09735](https://arxiv.org/abs/2605.09735).
- Summary: A runtime that decouples logical KV histories from physical storage beneath a static-graph (CUDA-graph-style) decoder. A block pager tracks active KV state; each decode step is materialized via a single committed descriptor; a merge-staged transport path coalesces non-contiguous KV mappings into a few large transfer groups feeding a fixed-shape attention kernel — reconciling static-graph efficiency with irregular online decoding (variable lengths, async EOS, fragmenting histories).
- Novelty: High. Targets an under-explored systems gap: CUDA-graph decoders assume fixed shapes, but real online decoding fragments KV histories. The "logical/physical KV decoupling under a static graph" framing is novel and directly relevant to high-throughput production decoders.
- Impact: Potentially high for production decoders that depend on CUDA graphs (most high-throughput vLLM/TRT-LLM configs).
- Runtime evaluation: Partial / unclear. Described as a runtime design; specific TTFT/TBT/throughput numbers versus vLLM/SGLang were not visible in the available text. Flagged for a deeper PDF read once arXiv access is restored — the gap, not an exclusion.

---

## Additional context (noted this week, not selected as top entries)

- **Algorithmic KV quantization/eviction, accuracy-led, little-to-no serving evaluation:**
  - [RateQuant — Optimal Mixed-Precision KV Cache Quantization via Rate-Distortion Theory (arXiv:2605.06675)](https://arxiv.org/abs/2605.06675): per-quantizer distortion-model fitting + closed-form reverse-waterfilling bit allocation; zero inference overhead, 1.6 s calibration, KIVI PPL 49.3→14.9 on Qwen3-8B at 2.5 avg bits. Clean theory; no serving benchmark. (v1 dated Apr 22, first in May listing, not previously reported.)
  - [FibQuant — Universal Vector Quantization for Random-Access KV-Cache Compression (arXiv:2605.11478)](https://arxiv.org/abs/2605.11478): shared radial-angular codebook preserving random access; strong fidelity/compression on GPT-2-small/TinyLlama only; no latency measurement.
  - [LaProx — Reformulating KV Cache Eviction for Long-Context LLM Inference (arXiv:2605.07234)](https://arxiv.org/abs/2605.07234): output-aware layer-wise eviction scoring; SOTA accuracy retention down to ~5% cache on LongBench; accuracy-only.
  - [LKV — End-to-End Learning of Head-wise Budgets and Token Selection (arXiv:2605.06676)](https://arxiv.org/abs/2605.06676): differentiable per-head budget + selection; SOTA on LongBench/RULER; accuracy-only. (April v1, May listing, not previously reported.)
  - [Make Each Token Count — Improving Long-Context Performance with KV Cache Eviction (arXiv:2605.09649)](https://arxiv.org/abs/2605.09649): learned global cross-layer/modality retention gates; argues eviction can beat full-cache attention; accuracy/memory-led, no serving metrics. Code at github.com/ngocbh/trimkv.
  - [KV-Fold — One-Step KV-Cache Recurrence for Long-Context Inference (arXiv:2605.12471)](https://arxiv.org/abs/2605.12471): training-free `foldl`-over-chunks framing of chunked recurrence; stability/quality analysis, no throughput/latency.
- **Out-of-window but cache-rich venue lists (reference points, conference weeks fall outside the window):** [FAST '26 spring accepted papers](https://www.usenix.org/conference/fast26/spring-accepted-papers) (CacheSlide cross-position KV reuse, Bidaw two-tier KV, IMPRESS multi-tier prefix storage, programmable page cache for model loading); [NSDI '26 spring accepted papers](https://www.usenix.org/conference/nsdi26/spring-accepted-papers) (DroidSpeak cross-model distributed KV reuse ~4× throughput, SYMPHONY, Cortex cross-region semantic cache); [SIGMOD 2026 accepted papers](https://2026.sigmod.org/sigmod_papers.shtml) (Beluga CXL KVCache architecture, KVDrive multi-tier KV management, Prefix→Fusion RAG cache); [EuroSys 2026 papers](https://2026.eurosys.org/papers.html) (KUNSERVE parameter-centric memory mgmt; Best Paper *PaCaR*, page-cache replication on NUMA — a notable cache-systems best paper, though page cache not KV cache); ASPLOS 2026 (REPA/STARC PIM KV offload).
- **Boundary, just before window (not re-reported):** [Databricks — How Superhuman and Databricks built a 200K QPS inference platform (May 8, 2026)](https://www.databricks.com/blog/how-superhuman-and-databricks-built-200k-qps-inference-platform-together) — 200K+ QPS, 60% throughput gain, sub-second P99 via FP8 + attention-kernel optimization; just before the window and adjacent to prior-week coverage.
- **Explicit negatives this week:** No new in-window non-LLM distributed/storage-system caching papers (CacheLib/CDN/object-cache); no new in-window semantic/RAG-caching papers; no new dedicated cross-instance/disaggregated distributed-KV system beyond Tutti and KV-RM. Vendor blogs checked with nothing in window: SGLang, TensorRT-LLM, NVIDIA Dynamo, llm-d, KServe, Red Hat AI, Together AI, Fireworks, Anyscale, Databricks Mosaic, Modal, Replicate, RunPod, CoreWeave, VAST Data, Pure Storage, WEKA, CacheLib, RocksDB, Ceph, MinIO, AWS/Azure/Google Cloud storage blogs. Frontier AI lab blogs/tech reports checked with nothing in window: Anthropic, OpenAI, Google DeepMind/Google Research, Meta AI/FAIR, xAI, Mistral, Cohere, NVIDIA Research, Microsoft Research, DeepSeek, Moonshot/Kimi, Qwen/Alibaba, Zhipu/GLM, 01.AI, Apple, Amazon Science, Tencent AI Lab, ByteDance Seed, Baidu Research (Grok 4.3 sliding/global attention, DeepSeek V4, GLM-5.1, Kimi K2.6 all predate the window). Venues with no in-window content: OSDI/ATC/USENIX Security/SOSP/SIGCOMM/HotNets/HotOS/HotStorage/SoCC 2026 (no accepted lists yet), VLDB 2026, NeurIPS/ICLR/ICML 2026. Curated trackers (TreeAI-Lab/Awesome-KV-Cache-Management, jjiantong/Awesome-KV-Cache-Optimization, October2001/Awesome-KV-Cache-Compression) and Hacker News / Papers We Love: checked via web search (GitHub commit-history access was unavailable); no confirmed additions or discussions dated within the window.

---

## Cross-cutting observations (this week)

- **The low-bit-KV pendulum swings back to "measure first."** The prior fortnight's headline was TurboQuant-2-bit becoming a vLLM default; this week the vLLM team itself (entry #1) published a comprehensive study concluding FP8 is still the right default and TurboQuant only earns its keep under genuine memory pressure. Paired with LMCache's MI300X benchmark (entry #2) — which shows synthetic fixed-hit-rate harnesses *invert* the conclusion — the week's real theme is empirical hygiene: the field is correcting its own recent defaults with first-party measurement rather than more mechanisms.
- **Cache evaluation methodology is itself becoming a contribution.** LMCache's `PYTHONHASHSEED` caveat and explicit low-load crossover (HBM prefix cache wins until the working set exceeds L1) are the kind of negative results that rarely get published but silently corrupt cache A/B tests. Expect "how to benchmark a KV-cache tier honestly" to recur.
- **The SSD/DRAM KV tier is consolidating into real storage-system engineering.** Tutti (entry #5, GPU-direct SSD object store), Fluxion (entry #7, CPU-GPU sparse-attention pipelining), and vLLM v0.21.0's HMA+Mooncake connector consolidation (entry #3) are three views of the same shift: the KV cache hierarchy is now designed with storage-systems primitives (object stores, direct I/O, slack-aware scheduling), not ad-hoc spill paths. This continues the GhostServe/erasure-coding and DDN/Lustre threads from prior weeks.
- **Agentic caching is splitting into "predict" vs. "schedule" vs. "store."** PBKV (entry #6, predict reuse for dynamic workflows) sits next to the prior-week SAGA (schedule whole workflows) and Mooncake (store cross-instance) — a clean three-way decomposition of the agent-serving cache problem that is starting to stabilize as the field's mental model.
- **Static-graph decoders meet dynamic KV.** KV-RM (entry #8) names a real and under-explored production tension — CUDA-graph fixed shapes vs. fragmenting online KV histories — that most academic eviction/quantization papers ignore entirely. It is the systems-side complement to the accuracy-led eviction cluster in Additional Context.

## References

- [vLLM Blog — A First Comprehensive Study of TurboQuant: Accuracy and Performance (May 11, 2026)](https://vllm.ai/blog/2026-05-11-turboquant)
- [LMCache Blog — Benchmarking LMCache for Multi-Turn Agentic Workloads on AMD MI300X (May 12, 2026)](https://blog.lmcache.ai/en/2026/05/12/benchmarking-lmcache-for-multi-turn-agentic-workloads-on-amd-mi300x/) · [LMCache — DeepSeek V4 and your wallet (May 4, 2026)](https://blog.lmcache.ai/en/2026/05/04/deepseek-v4-explained-and-why-it-matters-to-your-wallet/) · [LMCache on Amazon SageMaker HyperPod (Apr 22, 2026)](https://blog.lmcache.ai/en/2026/04/22/lmcache-on-amazon-sagemaker-hyperpod-accelerating-llm-inference-with-managed-tiered-kv-cache/)
- [vLLM v0.21.0 release notes (May 15, 2026)](https://github.com/vllm-project/vllm/releases/tag/v0.21.0) · [vLLM v0.20.2](https://github.com/vllm-project/vllm/releases/tag/v0.20.2) · [vLLM v0.20.1](https://github.com/vllm-project/vllm/releases/tag/v0.20.1) · [vLLM v0.20.0](https://github.com/vllm-project/vllm/releases/tag/v0.20.0)
- [MLSys 2026 papers page](https://mlsys.org/virtual/2026/papers.html) · [MorphServe (arXiv:2506.02006)](https://arxiv.org/abs/2506.02006)
- [arXiv:2605.03375 — Tutti: Making SSD-Backed KV Cache Practical for Long-Context LLM Serving](https://arxiv.org/abs/2605.03375)
- [arXiv:2605.06472 — PBKV: Efficient Serving for Dynamic Agent Workflows with Prediction-based KV-Cache Management](https://arxiv.org/abs/2605.06472)
- [arXiv:2605.07719 — Fluxion: An Efficient Hybrid Sparse Attention with CPU-GPU Parallelism for Long-Context Inference](https://arxiv.org/abs/2605.07719)
- [arXiv:2605.09735 — KV-RM: Regularizing KV-Cache Movement for Static-Graph LLM Serving](https://arxiv.org/abs/2605.09735)
- [arXiv:2605.06675 — RateQuant: Optimal Mixed-Precision KV Cache Quantization via Rate-Distortion Theory](https://arxiv.org/abs/2605.06675)
- [arXiv:2605.11478 — FibQuant: Universal Vector Quantization for Random-Access KV-Cache Compression](https://arxiv.org/abs/2605.11478)
- [arXiv:2605.07234 — LaProx: Reformulating KV Cache Eviction for Long-Context LLM Inference](https://arxiv.org/abs/2605.07234)
- [arXiv:2605.06676 — LKV: End-to-End Learning of Head-wise Budgets and Token Selection for KV Cache Eviction](https://arxiv.org/abs/2605.06676)
- [arXiv:2605.09649 — Make Each Token Count: Improving Long-Context Performance with KV Cache Eviction](https://arxiv.org/abs/2605.09649)
- [arXiv:2605.12471 — KV-Fold: One-Step KV-Cache Recurrence for Long-Context Inference](https://arxiv.org/abs/2605.12471)
- [FAST '26 spring accepted papers](https://www.usenix.org/conference/fast26/spring-accepted-papers) · [NSDI '26 spring accepted papers](https://www.usenix.org/conference/nsdi26/spring-accepted-papers) · [SIGMOD 2026 accepted papers](https://2026.sigmod.org/sigmod_papers.shtml) · [EuroSys 2026 papers](https://2026.eurosys.org/papers.html)
- [Databricks — How Superhuman and Databricks built a 200K QPS inference platform (May 8, 2026)](https://www.databricks.com/blog/how-superhuman-and-databricks-built-200k-qps-inference-platform-together)
</content>
</invoke>
