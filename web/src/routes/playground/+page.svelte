<script lang="ts">
  import { SearchSelect } from '@koblibs/ui';
  import type { SearchSelectItem } from '@koblibs/ui';
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import {
    searchShows,
    getShowDetail,
    playgroundSearch,
    playgroundCompare,
    addDecisionCorrection,
    listTestScenarios,
    runTestScenario,
    getPromptTemplate
  } from '$lib/api/client';
  import type {
    ShowDetail,
    SeasonDetail,
    ReviewDelta,
    ReviewResponse,
    SearchAndReviewResponse,
    CompareResponse,
    TestScenario,
    TestResult
  } from '$lib/types';
  import SamplingControls, {
    emptySampling,
    toSamplingConfig,
    type SamplingState
  } from '$lib/components/SamplingControls.svelte';

  // Per-run sampling override shared by Search & Compare tabs. Not
  // persisted — purely per-session tuning on top of the persisted
  // `sampling_review_candidates` default.
  let runSampling: SamplingState = $state(emptySampling());

  // Tab state
  let activeTab = $state<'search' | 'compare' | 'testbed' | 'custom'>('search');

  // Custom scenario tab state — paste a full ReviewRequest JSON and send it
  // straight to /api/v1/review-candidates. Useful for one-off "what if I
  // changed the wanted year" experiments without editing testbed source.
  const CUSTOM_REQUEST_TEMPLATE = `{
  "wanted_title": "Severance",
  "want": {
    "kind": "tv_episode",
    "season": "2",
    "episode": "7",
    "episode_title": "Chikhai Bardo",
    "air_date": "2025-02-21",
    "year": 2022
  },
  "quality_profile": {
    "max_resolution": "1080p",
    "preferred_sources": ["WEB-DL"],
    "rejected_terms": ["2160p", "x265", "season packs"]
  },
  "candidates": [
    {
      "name": "Severance.S02E07.Chikhai.Bardo.1080p.ATVP.WEB-DL.DDP5.1.Atmos.H.264-FLUX",
      "kobload_accepted": true,
      "kobload_score": 1.5,
      "parsed_quality": { "resolution": "1080p", "source": "WEB-DL", "codec": "H.264" },
      "publish_date": "2025-02-21",
      "seeders": 300
    },
    {
      "name": "Severance.S02E07.720p.HDTV.x264-FAKE",
      "kobload_accepted": false,
      "kobload_score": 0.4,
      "parsed_quality": { "resolution": "720p", "source": "HDTV", "codec": "x264" },
      "quality_rejected": true,
      "seeders": 0
    }
  ]
}`;
  let customRequestText = $state(CUSTOM_REQUEST_TEMPLATE);
  let customLoading = $state(false);
  let customError = $state('');
  let customResult = $state<ReviewResponse | null>(null);

  async function handleCustomRun() {
    customLoading = true;
    customError = '';
    customResult = null;
    let parsed: any;
    try {
      parsed = JSON.parse(customRequestText);
    } catch (e) {
      customError = 'Invalid JSON: ' + (e instanceof Error ? e.message : String(e));
      customLoading = false;
      return;
    }
    try {
      const { submitReview } = await import('$lib/api/client');
      customResult = await submitReview(parsed);
    } catch (e) {
      customError = e instanceof Error ? e.message : 'Request failed';
    }
    customLoading = false;
  }

  function resetCustomRequest() {
    customRequestText = CUSTOM_REQUEST_TEMPLATE;
    customError = '';
    customResult = null;
  }

  // Show picker state
  let selectedShowId = $state('');
  let showDetail = $state<ShowDetail | null>(null);
  let showDetailLoading = $state(false);

  // Season/episode picker state
  let selectedSeasonId = $state('');
  let selectedEpisodeId = $state('');

  // Search & Review state
  let searchLoading = $state(false);
  let searchResults = $state<SearchAndReviewResponse | null>(null);
  let searchError = $state('');

  // Compare state
  let releaseText = $state('');
  let publishDate = $state(new Date().toISOString().slice(0, 10));
  let compareLoading = $state(false);
  let compareResults = $state<CompareResponse | null>(null);
  let compareError = $state('');
  let showComparePrompt = $state(false);

  // Correction modal
  type CorrectionTarget = { delta: ReviewDelta; bucket: 'agreed' | 'vetoed' | 'promoted' };
  let correctionTarget = $state<CorrectionTarget | null>(null);
  let correctionDecision = $state('');
  let correctionReason = $state('');
  let addingCorrection = $state(false);

  // Derived: seasons as SearchSelectItems (value = UUID for build-media-want, fallback to identifier)
  const seasonItems = $derived<SearchSelectItem[]>(
    showDetail?.seasons.map((s) => ({
      value: s.id || s.season_identifier,
      label: `Season ${s.season_identifier}`
    })) ?? []
  );

  // Derived: selected season detail
  const selectedSeason = $derived<SeasonDetail | undefined>(
    showDetail?.seasons.find((s) => (s.id || s.season_identifier) === selectedSeasonId)
  );

  // Derived: episodes as SearchSelectItems (value = UUID for build-media-want, fallback to identifier)
  const episodeItems = $derived<SearchSelectItem[]>(
    selectedSeason?.episodes.map((e) => ({
      value: e.id || e.episode_identifier,
      label: `Episode ${e.episode_identifier}${e.title ? ` \u2014 ${e.title}` : ''}`,
      description: e.air_date ?? undefined
    })) ?? []
  );

  // Derived: media type from show detail
  const mediaType = $derived(showDetail?.content_category ?? 'tv');

  // Show search function for SearchSelect
  async function searchShowsFn(query: string): Promise<SearchSelectItem[]> {
    const results = await searchShows(query);
    return results.map((s) => ({
      value: s.id,
      label: `${s.title}${s.year ? ` (${s.year})` : ''}`
    }));
  }

  // Handle show selection
  async function handleShowChange(value: string) {
    selectedShowId = value;
    selectedSeasonId = '';
    selectedEpisodeId = '';
    showDetail = null;
    if (!value) return;
    showDetailLoading = true;
    try {
      showDetail = await getShowDetail(value);
    } catch {
      showDetail = null;
    }
    showDetailLoading = false;
  }

  // Handle season selection
  function handleSeasonChange(value: string) {
    selectedSeasonId = value;
    selectedEpisodeId = '';
  }

  // Search & Review
  async function handleSearch() {
    if (!selectedShowId) return;
    searchLoading = true;
    searchError = '';
    searchResults = null;
    try {
      const cfg = toSamplingConfig(runSampling);
      searchResults = await playgroundSearch({
        show_id: selectedShowId,
        season_id: selectedSeasonId || undefined,
        episode_ids: selectedEpisodeId ? [selectedEpisodeId] : [],
        sampling: Object.keys(cfg).length === 0 ? undefined : cfg
      });
    } catch (e) {
      searchError = e instanceof Error ? e.message : 'Search failed';
    }
    searchLoading = false;
  }

  // Compare
  async function handleCompare() {
    const lines = releaseText
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l.length > 0);
    if (!selectedShowId || lines.length === 0) {
      compareError = 'Select a show and paste at least one release name.';
      return;
    }
    compareLoading = true;
    compareError = '';
    compareResults = null;
    showComparePrompt = false;

    const selectedEp = selectedSeason?.episodes.find(
      (e) => (e.id || e.episode_identifier) === selectedEpisodeId
    );

    try {
      const cfg = toSamplingConfig(runSampling);
      compareResults = await playgroundCompare({
        source_id: selectedShowId || undefined,
        media_type: mediaType,
        wanted_title: showDetail?.title ?? '',
        wanted_year: showDetail?.year,
        wanted_season: selectedSeason?.season_identifier || undefined,
        wanted_episode: selectedEp?.episode_identifier || undefined,
        wanted_episode_title: selectedEp?.title || undefined,
        releases: lines.map((name) => ({
          name,
          media_kind: mediaType,
          publish_date: publishDate || undefined
        })),
        sampling: Object.keys(cfg).length === 0 ? undefined : cfg
      });
    } catch (e) {
      compareError = e instanceof Error ? e.message : 'Compare failed';
    }
    compareLoading = false;
  }

  function formatSize(bytes?: number): string {
    if (bytes == null) return '-';
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  function openCorrectionForm(delta: ReviewDelta, bucket: 'agreed' | 'vetoed' | 'promoted') {
    correctionTarget = { delta, bucket };
    correctionDecision = '';
    correctionReason = '';
  }

  /// The LLM "decision" for this delta — `agreed`/`promoted` mean the LLM picked it,
  /// `vetoed` means it didn't.
  function modelDecisionFor(bucket: 'agreed' | 'vetoed' | 'promoted'): string {
    return bucket === 'vetoed' ? 'reject' : 'accept';
  }

  async function handleAddCorrection() {
    if (!correctionTarget || !correctionDecision) return;
    addingCorrection = true;
    try {
      await addDecisionCorrection({
        release_name: correctionTarget.delta.name,
        entity_id: selectedShowId || undefined,
        model_decision: modelDecisionFor(correctionTarget.bucket),
        correct_decision: correctionDecision,
        reason: correctionReason
      });
      correctionTarget = null;
    } catch (e) {
      compareError = e instanceof Error ? e.message : 'Failed to add correction';
    }
    addingCorrection = false;
  }

  // ── Testbed state ──────────────────────────────────────────────────

  let scenarios = $state<TestScenario[]>([]);
  let scenariosLoading = $state(false);
  let scenariosError = $state('');
  let scenariosLoaded = $state(false);

  let testResult = $state<TestResult | null>(null);
  let testRunning = $state<string | null>(null);
  let testError = $state('');
  let testPromptOverride = $state('');
  let showTestPromptEditor = $state(false);

  // Run All state
  let runAllActive = $state(false);
  let runAllProgress = $state(0);
  let runAllTotal = $state(0);
  let runAllResults = $state<Map<string, TestResult>>(new Map());

  // Group scenarios by category
  const scenariosByCategory = $derived.by<Map<string, TestScenario[]>>(() => {
    const grouped = new Map<string, TestScenario[]>();
    for (const s of scenarios) {
      const cat = s.category || 'Uncategorized';
      if (!grouped.has(cat)) grouped.set(cat, []);
      grouped.get(cat)!.push(s);
    }
    return grouped;
  });

  async function loadScenarios() {
    if (scenariosLoaded) return;
    scenariosLoading = true;
    scenariosError = '';
    try {
      scenarios = await listTestScenarios();
      scenariosLoaded = true;
    } catch (e) {
      scenariosError = e instanceof Error ? e.message : 'Failed to load scenarios';
    }
    scenariosLoading = false;
  }

  async function handleRunScenario(scenario: TestScenario) {
    testRunning = scenario.id;
    testError = '';
    testResult = null;
    try {
      testResult = await runTestScenario(scenario.id, testPromptOverride || undefined);
    } catch (e) {
      testError = e instanceof Error ? e.message : 'Failed to run scenario';
    }
    testRunning = null;
  }

  async function handleRunAll() {
    runAllActive = true;
    runAllResults = new Map();
    runAllProgress = 0;
    runAllTotal = scenarios.length;
    testResult = null;
    testError = '';
    for (const scenario of scenarios) {
      runAllProgress++;
      try {
        const result = await runTestScenario(scenario.id, testPromptOverride || undefined);
        runAllResults = new Map([...runAllResults, [scenario.id, result]]);
      } catch {
        // skip failed scenarios in the summary
      }
    }
    runAllActive = false;
  }

  // Load scenarios when testbed tab is activated
  $effect(() => {
    if (activeTab === 'testbed') {
      loadScenarios();
    }
  });
</script>

<!-- ── Reusable snippets ─────────────────────────────────────────────────── -->

{#snippet notesBanner(notes: string | undefined | null)}
  {#if notes}
    <div
      class="notes-banner"
      class:notes-warning={notes.startsWith('⚠') || notes.startsWith('LLM')}
    >
      <div class="notes-label">LLM Notes</div>
      <p class="notes-text">{notes}</p>
    </div>
  {/if}
{/snippet}

{#snippet statsBadge(review: ReviewResponse)}
  {@const candidateCount = review.agreed.length + review.vetoed.length + review.promoted.length}
  <div class="stats-badge">
    {#if review.prompt_tokens != null}
      <span class="stat"
        ><span class="stat-label">in</span>
        <span class="stat-value">{review.prompt_tokens.toLocaleString()}</span> tok</span
      >
    {/if}
    {#if review.completion_tokens != null}
      <span class="stat"
        ><span class="stat-label">out</span>
        <span class="stat-value">{review.completion_tokens}</span> tok</span
      >
    {/if}
    {#if review.llm_inference_ms != null}
      <span class="stat"
        ><span class="stat-label">llm</span>
        <span class="stat-value">{(review.llm_inference_ms / 1000).toFixed(1)}</span> s</span
      >
    {/if}
    {#if review.context_assembly_ms != null}
      <span class="stat"
        ><span class="stat-label">ctx</span>
        <span class="stat-value">{review.context_assembly_ms}</span> ms</span
      >
    {/if}
    <span class="stat"
      ><span class="stat-label">surfaced</span>
      <span class="stat-value">{candidateCount}</span></span
    >
    {#if review.dropped_indices && review.dropped_indices.length > 0}
      <span class="stat stat-warning"
        ><span class="stat-label">dropped idx</span>
        <span class="stat-value">{review.dropped_indices.length}</span></span
      >
    {/if}
  </div>
{/snippet}

{#snippet bucketColumn(
  title: string,
  kind: 'agreed' | 'vetoed' | 'promoted',
  deltas: ReviewDelta[],
  correctable: boolean,
  expectedAccept: Set<string> | null,
  expectedReject: Set<string> | null
)}
  <div class="bucket-col bucket-{kind}">
    <div class="bucket-header">
      <span class="bucket-title">{title}</span>
      <span class="bucket-count">{deltas.length}</span>
    </div>
    {#if deltas.length === 0}
      <div class="bucket-empty">—</div>
    {:else}
      <div class="bucket-list">
        {#each deltas as delta (delta.idx)}
          {@const llmAccept = kind === 'agreed' || kind === 'promoted'}
          {@const expected = expectedAccept?.has(delta.name)
            ? 'accept'
            : expectedReject?.has(delta.name)
              ? 'reject'
              : null}
          {@const matchesExpected =
            expected == null ? null : expected === 'accept' ? llmAccept : !llmAccept}
          <div
            class="bucket-card"
            class:expected-pass={matchesExpected === true}
            class:expected-fail={matchesExpected === false}
          >
            <div class="bucket-card-name">
              {#if matchesExpected === true}
                <span class="exp-marker pass" title="Matches expected {expected}">✓</span>
              {:else if matchesExpected === false}
                <span class="exp-marker fail" title="Expected {expected}">✗</span>
              {/if}
              {delta.name}
            </div>
            {#if delta.reason}
              <div class="bucket-card-reason">{delta.reason}</div>
            {/if}
            {#if correctable}
              <button
                class="link-btn small"
                type="button"
                onclick={() => openCorrectionForm(delta, kind)}
              >
                Add Correction
              </button>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

{#snippet debugPanels(review: ReviewResponse)}
  <div class="debug-panels">
    {#if review.dropped_indices && review.dropped_indices.length > 0}
      <div class="debug-warning">
        ⚠ {review.dropped_indices.length} out-of-range index/indices: {review.dropped_indices.join(
          ', '
        )}
      </div>
    {/if}
    {#if review.accepts && review.accepts.length > 0}
      <details class="debug-details">
        <summary>Parsed accepts ({review.accepts.length})</summary>
        <table class="debug-table">
          <thead>
            <tr><th>idx</th><th>reason</th></tr>
          </thead>
          <tbody>
            {#each review.accepts as a (a.idx)}
              <tr><td class="mono">{a.idx}</td><td>{a.reason}</td></tr>
            {/each}
          </tbody>
        </table>
      </details>
    {/if}
    {#if review.raw_llm_response}
      <details class="debug-details">
        <summary>Raw LLM response ({review.raw_llm_response.length} chars)</summary>
        <JsonBlock content={review.raw_llm_response} label="Raw LLM Response" />
      </details>
    {/if}
    {#if review.debug_prompt}
      <details class="debug-details">
        <summary>Assembled prompt sent to LLM</summary>
        <JsonBlock content={review.debug_prompt} label="Prompt" />
      </details>
    {/if}
  </div>
{/snippet}

{#snippet diffView(
  review: ReviewResponse,
  correctable: boolean,
  expectedAccept: Set<string> | null = null,
  expectedReject: Set<string> | null = null
)}
  {@render notesBanner(review.notes)}
  {@render statsBadge(review)}
  <div class="bucket-grid">
    {@render bucketColumn(
      'Agreed',
      'agreed',
      review.agreed,
      correctable,
      expectedAccept,
      expectedReject
    )}
    {@render bucketColumn(
      'Vetoed',
      'vetoed',
      review.vetoed,
      correctable,
      expectedAccept,
      expectedReject
    )}
    {@render bucketColumn(
      'Promoted',
      'promoted',
      review.promoted,
      correctable,
      expectedAccept,
      expectedReject
    )}
  </div>
  {@render debugPanels(review)}
{/snippet}

<div class="playground-page">
  <!-- Tab bar -->
  <div class="tab-bar">
    <button
      class="tab-btn"
      class:active={activeTab === 'search'}
      type="button"
      onclick={() => (activeTab = 'search')}
    >
      Search & Review
    </button>
    <button
      class="tab-btn"
      class:active={activeTab === 'compare'}
      type="button"
      onclick={() => (activeTab = 'compare')}
    >
      Compare Releases
    </button>
    <button
      class="tab-btn"
      class:active={activeTab === 'testbed'}
      type="button"
      onclick={() => (activeTab = 'testbed')}
    >
      Testbed
    </button>
    <button
      class="tab-btn"
      class:active={activeTab === 'custom'}
      type="button"
      onclick={() => (activeTab = 'custom')}
    >
      Custom JSON
    </button>
  </div>

  {#if activeTab === 'custom'}
    <div class="custom-container">
      <div class="custom-header">
        <h3>Custom ReviewRequest</h3>
        <p class="custom-hint">
          Paste a full <code>ReviewRequest</code> JSON below and POST it directly to
          <code>/api/v1/review-candidates</code>. Useful for one-off experiments and reproducing
          weird inputs you can't easily generate via the Search or Compare tabs. The result is
          rendered with the same diff / debug panels as the rest of the playground.
        </p>
      </div>
      <div class="custom-body">
        <div class="custom-input-col">
          <textarea class="custom-textarea" bind:value={customRequestText} spellcheck="false"
          ></textarea>
          <div class="custom-actions">
            <button
              class="btn-primary"
              type="button"
              onclick={handleCustomRun}
              disabled={customLoading || !customRequestText.trim()}
            >
              {customLoading ? 'Running…' : 'Run review'}
            </button>
            <button
              class="btn-sec"
              type="button"
              onclick={resetCustomRequest}
              disabled={customLoading}
            >
              Reset to template
            </button>
          </div>
          {#if customError}
            <div class="error-banner" style="margin-top: 12px;">{customError}</div>
          {/if}
        </div>
        <div class="custom-result-col">
          {#if customResult}
            {@render diffView(customResult, false)}
          {:else}
            <div class="custom-empty">Run a request to see the diff and debug panels here.</div>
          {/if}
        </div>
      </div>
    </div>
  {:else if activeTab === 'testbed'}
    <div class="testbed-container">
      <div class="testbed-header">
        <!-- Run All button -->
        <div class="testbed-toolbar">
          <button
            class="btn action-btn testbed-run-all"
            type="button"
            onclick={handleRunAll}
            disabled={runAllActive || scenariosLoading || scenarios.length === 0}
          >
            {#if runAllActive}
              <span class="spinner"></span>
              Running scenario {runAllProgress}/{runAllTotal}...
            {:else}
              Run All ({scenarios.length})
            {/if}
          </button>
          <button
            class="btn alt"
            type="button"
            onclick={async () => {
              showTestPromptEditor = !showTestPromptEditor;
              if (showTestPromptEditor && !testPromptOverride) {
                try {
                  const resp = await getPromptTemplate('default', 'system');
                  testPromptOverride = resp.body;
                } catch {
                  /* ignore */
                }
              }
            }}
          >
            {showTestPromptEditor ? 'Hide' : 'Edit'} Prompt
          </button>
        </div>

        {#if showTestPromptEditor}
          <div class="inline-prompt-editor">
            <div class="prompt-editor-header">
              <span class="muted"
                >Inline prompt override — changes here only affect this run, not the global setting.</span
              >
              {#if testPromptOverride}
                <button
                  class="btn alt small"
                  type="button"
                  onclick={() => (testPromptOverride = '')}>Clear (use default)</button
                >
              {/if}
            </div>
            <textarea
              class="prompt-textarea"
              bind:value={testPromptOverride}
              rows="12"
              placeholder="Leave empty to use saved/default prompt. Paste a custom prompt here to test it."
            ></textarea>
            {#if testResult?.review.debug_prompt}
              <details class="prompt-details" style="margin-top: 8px;">
                <summary class="muted">Last prompt sent to LLM</summary>
                <JsonBlock content={testResult.review.debug_prompt} label="Prompt" />
              </details>
            {/if}
          </div>
        {/if}
      </div>

      <div class="testbed-body">
        {#if scenariosLoading}
          <div class="loading-state">
            <div class="spinner large"></div>
            <p>Loading scenarios...</p>
          </div>
        {:else if scenariosError}
          <div class="error-banner">{scenariosError}</div>
        {:else}
          <div class="testbed-left">
            <!-- Run All summary grid -->
            {#if runAllResults.size > 0}
              <div class="results-section">
                <h3>Run All Summary</h3>
                <div class="runall-grid">
                  {#each scenarios as scenario (scenario.id)}
                    {@const result = runAllResults.get(scenario.id)}
                    {@const totalCorrect = result
                      ? result.scoring.agreed_correct +
                        result.scoring.promoted_correct +
                        result.scoring.vetoed_correct +
                        result.scoring.silent_correct
                      : 0}
                    {@const totalExpected = result ? result.expected.length : 0}
                    {@const wrongCount = result ? result.scoring.wrong.length : 0}
                    <div
                      class="runall-cell"
                      class:runall-pass={result && wrongCount === 0}
                      class:runall-fail={result && wrongCount > 0}
                      class:runall-pending={!result}
                    >
                      <span class="runall-name">{scenario.name}</span>
                      {#if result}
                        <span class="runall-score">{totalCorrect}/{totalExpected}</span>
                      {:else}
                        <span class="runall-score">--</span>
                      {/if}
                    </div>
                  {/each}
                </div>
              </div>
            {/if}

            <!-- Scenario cards grouped by category -->
            {#each [...scenariosByCategory.entries()] as [category, categoryScenarios] (category)}
              <div class="results-section">
                <h3>{category}</h3>
                <div class="scenario-cards">
                  {#each categoryScenarios as scenario (scenario.id)}
                    <div class="scenario-card">
                      <div class="scenario-card-body">
                        <div class="scenario-card-top">
                          <span class="scenario-name">{scenario.name}</span>
                          <span class="category-badge">{scenario.category}</span>
                        </div>
                        <p class="scenario-desc">{scenario.description}</p>
                        <div class="scenario-meta">
                          <span class="scenario-candidates"
                            >{scenario.candidate_count} candidate{scenario.candidate_count === 1
                              ? ''
                              : 's'}</span
                          >
                        </div>
                      </div>
                      <button
                        class="btn-sec compact scenario-run-btn"
                        type="button"
                        onclick={() => handleRunScenario(scenario)}
                        disabled={testRunning != null || runAllActive}
                      >
                        {#if testRunning === scenario.id}
                          <span class="spinner"></span>
                          Running...
                        {:else}
                          Run
                        {/if}
                      </button>
                    </div>
                  {/each}
                </div>
              </div>
            {/each}
          </div>

          <div class="testbed-right">
            <!-- Single scenario result -->
            {#if testRunning}
              <div class="loading-state">
                <div class="spinner large"></div>
                <p>Running scenario...</p>
              </div>
            {:else if testError}
              <div class="error-banner">{testError}</div>
            {:else if testResult}
              {@const totalExpected = testResult.expected.length}
              {@const totalCorrect =
                testResult.scoring.agreed_correct +
                testResult.scoring.promoted_correct +
                testResult.scoring.vetoed_correct +
                testResult.scoring.silent_correct}
              {@const allPassed = testResult.scoring.wrong.length === 0}
              {@const expectedAcceptSet = new Set(
                testResult.expected
                  .filter((e) => e.expected.toLowerCase() === 'accept')
                  .map((e) => e.release_name)
              )}
              {@const expectedRejectSet = new Set(
                testResult.expected
                  .filter((e) => e.expected.toLowerCase() === 'reject')
                  .map((e) => e.release_name)
              )}
              <div class="results-section">
                <h3>Results: {testResult.scenario_name}</h3>

                <!-- Pass/fail summary -->
                <div
                  class="testbed-summary"
                  class:summary-pass={allPassed}
                  class:summary-fail={!allPassed}
                >
                  <span class="testbed-summary-count">{totalCorrect}/{totalExpected}</span>
                  <span class="testbed-summary-label">{allPassed ? 'PASSED' : 'FAILED'}</span>
                </div>

                <!-- Four-bucket scoring breakdown -->
                <div class="scoring-breakdown">
                  <div class="scoring-cell scoring-agreed">
                    <span class="scoring-num">{testResult.scoring.agreed_correct}</span>
                    <span class="scoring-label">agreed&nbsp;✓</span>
                  </div>
                  <div class="scoring-cell scoring-promoted">
                    <span class="scoring-num">{testResult.scoring.promoted_correct}</span>
                    <span class="scoring-label">promoted&nbsp;✓</span>
                  </div>
                  <div class="scoring-cell scoring-vetoed">
                    <span class="scoring-num">{testResult.scoring.vetoed_correct}</span>
                    <span class="scoring-label">vetoed&nbsp;✓</span>
                  </div>
                  <div class="scoring-cell scoring-silent">
                    <span class="scoring-num">{testResult.scoring.silent_correct}</span>
                    <span class="scoring-label">silent&nbsp;✓</span>
                  </div>
                  {#if testResult.scoring.wrong.length > 0}
                    <div class="scoring-cell scoring-wrong">
                      <span class="scoring-num">{testResult.scoring.wrong.length}</span>
                      <span class="scoring-label">wrong&nbsp;✗</span>
                    </div>
                  {/if}
                </div>

                {#if testResult.scoring.wrong.length > 0}
                  <div class="wrong-list">
                    <div class="wrong-list-title">Wrong verdicts</div>
                    {#each testResult.scoring.wrong as w, i (i)}
                      <div
                        class="wrong-row"
                        class:wrong-missed={w.sub_reason === 'missed'}
                        class:wrong-spurious={w.sub_reason === 'spurious'}
                      >
                        <span class="wrong-tag">{w.sub_reason}</span>
                        <span class="wrong-name">{w.release_name}</span>
                        <span class="wrong-meta"
                          >expected {w.expected} → actually {w.actual_bucket}</span
                        >
                      </div>
                    {/each}
                  </div>
                {/if}
              </div>

              <!-- Diff: agreed / vetoed / promoted, with ✓/✗ overlay against expected_actions -->
              <div class="results-section">
                <h3>LLM Diff vs kobload</h3>
                {@render diffView(testResult.review, false, expectedAcceptSet, expectedRejectSet)}
              </div>

              <!-- Timing -->
              {#if testResult.review.context_assembly_ms != null || testResult.review.llm_inference_ms != null}
                <div class="results-section">
                  <h3>Timing</h3>
                  <div class="timing-row">
                    {#if testResult.review.context_assembly_ms != null}
                      <div class="timing-item">
                        <span class="timing-label">Context Assembly</span>
                        <span class="timing-value">{testResult.review.context_assembly_ms}ms</span>
                      </div>
                    {/if}
                    {#if testResult.review.llm_inference_ms != null}
                      <div class="timing-item">
                        <span class="timing-label">LLM Inference</span>
                        <span class="timing-value">{testResult.review.llm_inference_ms}ms</span>
                      </div>
                    {/if}
                  </div>
                </div>
              {/if}

              <!-- Session link -->
              {#if testResult.session_id}
                <div class="results-section">
                  <a href="/history?session={testResult.session_id}" class="session-link"
                    >View Session {testResult.session_id.slice(0, 8)}...</a
                  >
                </div>
              {/if}

              <!-- (Debug prompt + raw LLM response are now part of diffView's debug-panels.) -->
            {/if}
          </div>
        {/if}
      </div>
    </div>
  {:else}
    <div class="playground-layout">
      <!-- Left panel: Input -->
      <div class="panel input-panel">
        <div class="panel-scroll">
          <!-- Show picker -->
          <section class="form-section">
            <h3>Show</h3>
            <div class="form-group">
              <label class="form-label" for="show-picker">Show / Movie</label>
              <SearchSelect
                id="show-picker"
                searchFn={searchShowsFn}
                value={selectedShowId}
                placeholder="Search for a show..."
                onchange={(value) => handleShowChange(value)}
              />
            </div>
            {#if showDetailLoading}
              <div class="detail-loading">Loading show details...</div>
            {/if}
            {#if showDetail}
              <div class="show-info">
                <span class="show-info-title">{showDetail.title}</span>
                {#if showDetail.year}
                  <span class="show-info-year">({showDetail.year})</span>
                {/if}
                {#if showDetail.network}
                  <span class="show-info-meta">{showDetail.network}</span>
                {/if}
                <span class="show-info-meta">{showDetail.content_category}</span>
              </div>
            {/if}
          </section>

          <!-- Season picker -->
          <section class="form-section">
            <h3>Season & Episode</h3>
            <div class="form-group">
              <label class="form-label" for="season-picker">Season</label>
              <SearchSelect
                id="season-picker"
                items={seasonItems}
                value={selectedSeasonId}
                placeholder={showDetail ? 'Select season...' : 'Select a show first'}
                disabled={!showDetail}
                allowCustom={true}
                onchange={(value) => handleSeasonChange(value)}
              />
            </div>

            <div class="form-group">
              <label class="form-label" for="episode-picker">Episode</label>
              <SearchSelect
                id="episode-picker"
                items={episodeItems}
                value={selectedEpisodeId}
                placeholder={selectedSeasonId ? 'Select episode...' : 'Select a season first'}
                disabled={!selectedSeasonId}
                allowCustom={true}
                onchange={(value) => (selectedEpisodeId = value)}
              />
              <span class="field-hint">Leave empty to search whole season</span>
            </div>
          </section>

          <!-- Per-run sampling override (shared by Search & Compare) -->
          <section class="form-section">
            <SamplingControls
              bind:value={runSampling}
              label="Sampling (per-run override)"
              collapsible
              defaultExpanded={false}
            />
          </section>

          <!-- Tab-specific inputs -->
          {#if activeTab === 'search'}
            <button
              class="btn action-btn"
              type="button"
              onclick={handleSearch}
              disabled={searchLoading || !selectedShowId}
            >
              {#if searchLoading}
                <span class="spinner"></span>
                Searching...
              {:else}
                Search & Review
              {/if}
            </button>
          {:else}
            <section class="form-section">
              <h3>Releases</h3>
              <p class="section-hint">Paste release names, one per line.</p>
              <textarea
                class="form-textarea candidate-textarea"
                bind:value={releaseText}
                rows="6"
                placeholder="Show.S01E01.1080p.WEB-DL.x264-GROUP&#10;Show.S01E01.720p.HDTV.x265-OTHER&#10;Show.S01E01.2160p.WEB.DDP5.1.H.265-ELITE"
              ></textarea>
            </section>

            <section class="form-section">
              <div class="form-group">
                <label class="form-label" for="publish-date">Publish Date</label>
                <input id="publish-date" class="form-input" type="date" bind:value={publishDate} />
              </div>
            </section>

            <button
              class="btn action-btn"
              type="button"
              onclick={handleCompare}
              disabled={compareLoading || !selectedShowId}
            >
              {#if compareLoading}
                <span class="spinner"></span>
                Comparing...
              {:else}
                Compare
              {/if}
            </button>
          {/if}
        </div>
      </div>

      <!-- Right panel: Output -->
      <div class="panel output-panel">
        <div class="panel-scroll">
          {#if activeTab === 'search'}
            <!-- Search results -->
            {#if searchLoading}
              <div class="loading-state">
                <div class="spinner large"></div>
                <p>Searching releases...</p>
              </div>
            {:else if searchError}
              <div class="error-banner">{searchError}</div>
            {:else if searchResults}
              <!-- Summary cards -->
              <div class="summary-row">
                <div class="summary-card">
                  <span class="summary-label">Found</span>
                  <span class="summary-value">{searchResults.search.total_found}</span>
                </div>
                <div class="summary-card">
                  <span class="summary-label">Accepted</span>
                  <span class="summary-value accent">{searchResults.search.total_accepted}</span>
                </div>
                {#if searchResults.search.best_pick}
                  <div class="summary-card best">
                    <span class="summary-label">Best Pick</span>
                    <span class="summary-value-sm">{searchResults.search.best_pick.name}</span>
                  </div>
                {/if}
              </div>

              <!-- Results table -->
              {#if searchResults.search.candidates.length > 0}
                <div class="results-section">
                  <h3>Candidates</h3>
                  <div class="table-wrap">
                    <table>
                      <thead>
                        <tr>
                          <th>Name</th>
                          <th>Score</th>
                          <th>Quality</th>
                          <th>Group</th>
                          <th>Indexer</th>
                          <th>Size</th>
                          <th>Status</th>
                        </tr>
                      </thead>
                      <tbody>
                        {#each [...searchResults.search.candidates].sort((a, b) => b.total_score - a.total_score) as release (release.guid)}
                          <tr
                            class:best-row={searchResults.search.best_pick?.name === release.name}
                          >
                            <td class="release-name-cell">{release.name}</td>
                            <td class="score-cell">{release.total_score.toFixed(2)}</td>
                            <td>
                              <div class="quality-badges">
                                {#if release.quality.resolution}
                                  <span class="q-badge">{release.quality.resolution}</span>
                                {/if}
                                {#if release.quality.source}
                                  <span class="q-badge">{release.quality.source}</span>
                                {/if}
                                {#if release.quality.codec}
                                  <span class="q-badge">{release.quality.codec}</span>
                                {/if}
                              </div>
                            </td>
                            <td class="muted-text">{release.release_group || '-'}</td>
                            <td class="muted-text">{release.indexer || '-'}</td>
                            <td class="muted-text">{formatSize(release.size)}</td>
                            <td>
                              {#if release.accepted}
                                <span class="status-chip accepted">Accepted</span>
                              {:else}
                                <span
                                  class="status-chip rejected"
                                  title={release.reject_reason || ''}>Rejected</span
                                >
                              {/if}
                            </td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  </div>
                </div>
              {:else}
                <div class="empty-inline">No candidates returned.</div>
              {/if}

              <!-- LLM Review -->
              <div class="results-section" style="margin-top: 24px;">
                <h3>LLM Review</h3>
                {@render diffView(searchResults.review, true)}
              </div>

              <!-- Timing + prompt -->
              {#if searchResults.review.context_assembly_ms != null || searchResults.review.llm_inference_ms != null}
                <div class="timing-bar" style="margin-top: 12px;">
                  {#if searchResults.review.context_assembly_ms != null}
                    <span>Context: {searchResults.review.context_assembly_ms}ms</span>
                  {/if}
                  {#if searchResults.review.llm_inference_ms != null}
                    <span>LLM: {(searchResults.review.llm_inference_ms / 1000).toFixed(1)}s</span>
                  {/if}
                  {#if searchResults.session_id}
                    <a href="/history" class="session-link"
                      >Session: {searchResults.session_id.slice(0, 8)}…</a
                    >
                  {/if}
                </div>
              {/if}

              {#if searchResults.review.debug_prompt}
                <details class="prompt-details" style="margin-top: 12px;">
                  <summary>View assembled prompt</summary>
                  <JsonBlock content={searchResults.review.debug_prompt} label="Prompt" />
                </details>
              {/if}
            {:else}
              <div class="empty-output">
                <svg
                  viewBox="0 0 24 24"
                  width="48"
                  height="48"
                  fill="currentColor"
                  class="empty-icon"
                >
                  <path
                    d="M15.5 14h-.79l-.28-.27A6.471 6.471 0 0 0 16 9.5 6.5 6.5 0 1 0 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"
                  ></path>
                </svg>
                <p>Select a show and click Search & Review to find and score releases.</p>
              </div>
            {/if}
          {:else}
            <!-- Compare results -->
            {#if compareLoading}
              <div class="loading-state">
                <div class="spinner large"></div>
                <p>Running comparison...</p>
              </div>
            {:else if compareError}
              <div class="error-banner">{compareError}</div>
            {:else if compareResults}
              <!-- Interpreted releases -->
              {#if compareResults.interpreted.length > 0}
                <div class="results-section">
                  <h3>Interpreted Releases</h3>
                  <div class="table-wrap">
                    <table>
                      <thead>
                        <tr>
                          <th>Input</th>
                          <th>Title</th>
                          <th>Resolution</th>
                          <th>Source</th>
                          <th>Codec</th>
                          <th>Group</th>
                          <th>Confidence</th>
                        </tr>
                      </thead>
                      <tbody>
                        {#each compareResults.interpreted as rel, i (i)}
                          <tr>
                            <td class="release-name-cell">{rel.input}</td>
                            <td>{rel.title}</td>
                            <td>{rel.quality.resolution || '-'}</td>
                            <td>{rel.quality.source || '-'}</td>
                            <td>{rel.quality.codec || '-'}</td>
                            <td>{rel.release_group || '-'}</td>
                            <td>
                              <div class="confidence-inline">
                                <div class="confidence-bar-sm">
                                  <div
                                    class="confidence-fill-sm"
                                    style="width: {rel.confidence * 100}%"
                                  ></div>
                                </div>
                                <span>{Math.round(rel.confidence * 100)}%</span>
                              </div>
                            </td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  </div>
                </div>
              {/if}

              <!-- LLM Diff vs kobload -->
              <div class="results-section">
                <h3>LLM Diff vs kobload</h3>
                {@render diffView(compareResults.review, true)}
              </div>

              <!-- Timing -->
              {#if compareResults.review.context_assembly_ms != null || compareResults.review.llm_inference_ms != null}
                <div class="results-section">
                  <h3>Timing</h3>
                  <div class="timing-row">
                    {#if compareResults.review.context_assembly_ms != null}
                      <div class="timing-item">
                        <span class="timing-label">Context Assembly</span>
                        <span class="timing-value"
                          >{compareResults.review.context_assembly_ms}ms</span
                        >
                      </div>
                    {/if}
                    {#if compareResults.review.llm_inference_ms != null}
                      <div class="timing-item">
                        <span class="timing-label">LLM Inference</span>
                        <span class="timing-value">{compareResults.review.llm_inference_ms}ms</span>
                      </div>
                    {/if}
                  </div>
                </div>
              {/if}

              <!-- Session link -->
              {#if compareResults.session_id}
                <div class="results-section">
                  <a href="/history?session={compareResults.session_id}" class="session-link"
                    >View Session {compareResults.session_id.slice(0, 8)}...</a
                  >
                </div>
              {/if}

              <!-- Debug prompt -->
              {#if compareResults.review.debug_prompt}
                <div class="results-section">
                  <button
                    class="prompt-toggle"
                    type="button"
                    onclick={() => (showComparePrompt = !showComparePrompt)}
                  >
                    <h3>Assembled Prompt</h3>
                    <svg
                      class="chevron"
                      class:expanded={showComparePrompt}
                      viewBox="0 0 24 24"
                      width="16"
                      height="16"
                      fill="currentColor"
                    >
                      <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"></path>
                    </svg>
                  </button>
                  {#if showComparePrompt}
                    <JsonBlock content={compareResults.review.debug_prompt} label="Prompt" />
                  {/if}
                </div>
              {/if}
            {:else}
              <div class="empty-output">
                <svg
                  viewBox="0 0 24 24"
                  width="48"
                  height="48"
                  fill="currentColor"
                  class="empty-icon"
                >
                  <path
                    d="M9.4 16.6L4.8 12l4.6-4.6L8 6l-6 6 6 6 1.4-1.4zm5.2 0l4.6-4.6-4.6-4.6L16 6l6 6-6 6-1.4-1.4z"
                  ></path>
                </svg>
                <p>
                  Paste release names and click Compare to see how they're interpreted and reviewed.
                </p>
              </div>
            {/if}
          {/if}
        </div>
      </div>
    </div>
  {/if}
</div>

<!-- Add Correction Modal -->
{#if correctionTarget}
  <div class="modal-backdrop" role="presentation" onclick={() => (correctionTarget = null)}></div>
  <div class="modal-dialog">
    <div class="modal-header">
      <h3>Add Correction</h3>
      <button
        class="close-btn"
        type="button"
        aria-label="Close"
        onclick={() => (correctionTarget = null)}
      >
        <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
          <path
            d="M19 6.41L17.59 5 12 10.59 6.41 5 5 6.41 10.59 12 5 17.59 6.41 19 12 13.41 17.59 19 19 17.59 13.41 12z"
          ></path>
        </svg>
      </button>
    </div>
    <div class="modal-body">
      <label class="form-group">
        <span class="form-label">Release Name</span>
        <input class="form-input" type="text" value={correctionTarget.delta.name} disabled />
      </label>
      <label class="form-group">
        <span class="form-label">Model Decision</span>
        <input
          class="form-input"
          type="text"
          value={modelDecisionFor(correctionTarget.bucket)}
          disabled
        />
      </label>
      <label class="form-group">
        <span class="form-label">Correct Decision</span>
        <select class="form-select" bind:value={correctionDecision}>
          <option value="">Select...</option>
          <option value="accept">Accept</option>
          <option value="reject">Reject</option>
        </select>
      </label>
      <label class="form-group">
        <span class="form-label">Reason</span>
        <textarea
          class="form-textarea"
          bind:value={correctionReason}
          rows="3"
          placeholder="Why should this decision be different?"></textarea>
      </label>
    </div>
    <div class="modal-footer">
      <button class="btn-sec" type="button" onclick={() => (correctionTarget = null)}>Cancel</button
      >
      <button
        class="btn-primary"
        type="button"
        disabled={!correctionDecision || addingCorrection}
        onclick={handleAddCorrection}
      >
        {addingCorrection ? 'Adding...' : 'Add Correction'}
      </button>
    </div>
  </div>
{/if}

<style>
  .playground-page {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  /* Tab bar */
  .tab-bar {
    display: flex;
    gap: 0;
    border-bottom: 1px solid var(--ring);
    padding: 0 24px;
    flex-shrink: 0;
  }

  .tab-btn {
    padding: 14px 20px;
    border: none;
    background: transparent;
    color: var(--muted);
    font-size: 14px;
    font-weight: 600;
    cursor: pointer;
    border-bottom: 2px solid transparent;
    transition: all 0.15s;
    margin-bottom: -1px;
  }

  .tab-btn:hover {
    color: var(--ink);
  }

  .tab-btn.active {
    color: var(--accent);
    border-bottom-color: var(--accent);
  }

  /* Layout */
  .playground-layout {
    display: flex;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .panel {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }

  .panel-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 24px;
  }

  .input-panel {
    border-right: 1px solid var(--ring);
    max-width: 480px;
  }

  .output-panel {
    flex: 1;
  }

  /* Form */
  .form-section {
    margin-bottom: 20px;
  }

  .form-section h3 {
    margin: 0 0 12px;
    font-size: 14px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
  }

  .form-group {
    display: grid;
    gap: 6px;
    margin-bottom: 12px;
  }

  .section-hint {
    font-size: 12px;
    color: var(--muted);
    margin: -8px 0 12px;
  }

  .field-hint {
    font-size: 11px;
    color: var(--muted);
    font-style: italic;
  }

  .detail-loading {
    font-size: 12px;
    color: var(--accent);
    font-style: italic;
    margin-bottom: 12px;
  }

  .show-info {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    padding: 10px 14px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 10px;
    margin-bottom: 12px;
    font-size: 13px;
  }

  .show-info-title {
    font-weight: 600;
    color: var(--ink);
  }

  .show-info-year {
    color: var(--muted);
  }

  .show-info-meta {
    font-size: 11px;
    padding: 2px 8px;
    border-radius: 4px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }

  .candidate-textarea {
    font-family: monospace;
    font-size: 12px;
  }

  /* Action button */
  .action-btn {
    width: 100%;
    padding: 14px;
    font-size: 15px;
    font-weight: 700;
    border-radius: 14px;
    background: var(--accent);
    color: #1d130c;
    border: none;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    transition: all 0.2s;
  }

  .action-btn:not(:disabled):hover {
    filter: brightness(1.1);
    transform: translateY(-2px);
    box-shadow: 0 8px 20px rgba(0, 0, 0, 0.25);
  }

  .action-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Spinner */
  .spinner {
    display: inline-block;
    width: 16px;
    height: 16px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.75s linear infinite;
  }

  .spinner.large {
    width: 32px;
    height: 32px;
    border-width: 3px;
    color: var(--accent);
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  /* Output states */
  .loading-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 16px;
    padding: 80px 24px;
    color: var(--muted);
  }

  .empty-output {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 16px;
    padding: 80px 24px;
    text-align: center;
    color: var(--muted);
  }

  .empty-icon {
    opacity: 0.2;
  }

  .empty-output p {
    max-width: 360px;
    font-size: 14px;
    line-height: 1.5;
  }

  .empty-inline {
    padding: 20px;
    text-align: center;
    color: var(--muted);
    font-size: 13px;
  }

  .error-banner {
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger, #ef4444);
    font-size: 13px;
  }

  /* Summary cards */
  .summary-row {
    display: flex;
    gap: 12px;
    margin-bottom: 20px;
    flex-wrap: wrap;
  }

  .summary-card {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 16px 20px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 14px;
    min-width: 100px;
  }

  .summary-card.best {
    flex: 1;
    border-color: var(--accent);
    background: var(--accent-dim);
  }

  .summary-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .summary-value {
    font-size: 28px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }

  .summary-value.accent {
    color: var(--accent);
  }

  .summary-value-sm {
    font-size: 12px;
    font-family: monospace;
    word-break: break-all;
    line-height: 1.4;
  }

  /* Results */
  .results-section {
    margin-bottom: 24px;
  }

  .results-section h3 {
    margin: 0 0 12px;
    font-size: 14px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
  }

  .table-wrap {
    overflow-x: auto;
    border: 1px solid var(--ring);
    border-radius: 12px;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    text-align: left;
    padding: 10px 12px;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    border-bottom: 1px solid var(--ring);
    font-weight: 600;
    white-space: nowrap;
  }

  td {
    padding: 10px 12px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.04);
    vertical-align: middle;
  }

  :global([data-theme='light']) td {
    border-bottom: 1px solid rgba(0, 0, 0, 0.04);
  }

  .best-row {
    background: var(--accent-dim);
  }

  .release-name-cell {
    max-width: 280px;
    word-break: break-all;
    font-family: monospace;
    font-size: 11px;
  }

  .score-cell {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
    font-size: 13px;
    white-space: nowrap;
  }

  .muted-text {
    color: var(--muted);
    font-size: 12px;
  }

  .status-chip {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
    white-space: nowrap;
  }

  .status-chip.accepted {
    background: rgba(34, 197, 94, 0.15);
    color: var(--success);
  }

  .status-chip.rejected {
    background: rgba(239, 68, 68, 0.15);
    color: var(--danger, #ef4444);
  }

  .quality-badges {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
  }

  .q-badge {
    font-size: 10px;
    padding: 2px 6px;
    border-radius: 4px;
    background: var(--tag-bg);
    color: var(--tag-text);
    white-space: nowrap;
  }

  /* Confidence inline */
  .confidence-inline {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--muted);
  }

  .confidence-bar-sm {
    width: 60px;
    height: 4px;
    background: var(--ring);
    border-radius: 2px;
    overflow: hidden;
  }

  .confidence-fill-sm {
    height: 100%;
    background: var(--accent);
    border-radius: 2px;
  }

  /* Verdicts */
  .verdicts-list {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .verdict-card {
    padding: 16px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-left: 4px solid var(--muted);
    border-radius: 12px;
  }

  .verdict-header {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 8px;
  }

  .verdict-action {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 4px 10px;
    border-radius: 6px;
  }

  .confidence-bar {
    flex: 1;
    height: 6px;
    background: var(--ring);
    border-radius: 3px;
    overflow: hidden;
  }

  .confidence-fill {
    height: 100%;
    border-radius: 3px;
    transition: width 0.3s;
  }

  .confidence-label {
    font-size: 12px;
    color: var(--muted);
    min-width: 36px;
    text-align: right;
  }

  .verdict-name {
    font-family: monospace;
    font-size: 12px;
    color: var(--ink);
    word-break: break-all;
    margin-bottom: 8px;
  }

  .verdict-reason {
    font-size: 13px;
    color: var(--muted);
    line-height: 1.5;
    margin-bottom: 8px;
  }

  .verdict-flags {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 10px;
  }

  .flag-badge {
    font-size: 10px;
    padding: 2px 8px;
    border-radius: 999px;
    background: var(--tag-bg);
    color: var(--tag-text);
    font-weight: 500;
  }

  .add-correction-btn {
    padding: 6px 12px;
    font-size: 11px;
    border-radius: 6px;
    background: transparent;
    border: 1px solid var(--ring);
    color: var(--muted);
    cursor: pointer;
  }

  .add-correction-btn:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  /* Timing */
  .timing-row {
    display: flex;
    gap: 24px;
  }

  .timing-item {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .timing-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.3px;
  }

  .timing-value {
    font-size: 20px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }

  /* Session link */
  .session-link {
    font-size: 13px;
    color: var(--accent);
    text-decoration: none;
  }

  .session-link:hover {
    text-decoration: underline;
  }

  /* Prompt toggle */
  .prompt-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    background: transparent;
    border: none;
    color: var(--ink);
    cursor: pointer;
    padding: 0;
    width: 100%;
  }

  .prompt-toggle h3 {
    margin: 0;
  }

  .chevron {
    transition: transform 0.2s;
    color: var(--muted);
  }

  .chevron.expanded {
    transform: rotate(180deg);
  }

  /* Modal */
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 80;
  }

  .modal-dialog {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: 480px;
    max-width: 90vw;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    z-index: 90;
    box-shadow: var(--shadow);
  }

  .modal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px;
    border-bottom: 1px solid var(--ring);
  }

  .modal-header h3 {
    margin: 0;
    font-size: 16px;
  }

  .close-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    padding: 4px;
    border-radius: 6px;
  }

  .close-btn:hover {
    color: var(--ink);
  }

  .modal-body {
    padding: 20px;
  }

  .modal-footer {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    padding: 16px 20px;
    border-top: 1px solid var(--ring);
  }

  .compact {
    padding: 6px 12px;
    font-size: 11px;
  }

  /* Testbed */
  .testbed-container {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow: hidden;
  }

  .testbed-header {
    flex-shrink: 0;
    padding: 24px 24px 0 24px;
  }

  .testbed-body {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 320px minmax(0, 1fr);
    gap: 20px;
    padding: 16px 24px 24px 24px;
    overflow: hidden;
  }

  .testbed-left,
  .testbed-right {
    min-height: 0;
    overflow-y: auto;
    padding-right: 4px;
  }

  .testbed-right:empty::before {
    content: 'Run a scenario to see results.';
    display: block;
    color: var(--muted);
    font-size: 13px;
    padding: 24px;
    text-align: center;
    border: 1px dashed var(--ring);
    border-radius: 12px;
  }

  @media (max-width: 900px) {
    .testbed-body {
      grid-template-columns: minmax(0, 1fr);
      overflow-y: auto;
    }
    .testbed-left,
    .testbed-right {
      overflow: visible;
    }
  }

  .testbed-toolbar {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-bottom: 16px;
  }

  .testbed-run-all {
    width: auto;
    padding: 10px 24px;
    font-size: 13px;
  }

  .scenario-cards {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 10px;
  }

  .scenario-card {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    padding: 16px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
  }

  .scenario-card-body {
    flex: 1;
    min-width: 0;
  }

  .scenario-card-top {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
    flex-wrap: wrap;
  }

  .scenario-name {
    font-weight: 700;
    font-size: 14px;
    color: var(--ink);
  }

  .category-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 2px 8px;
    border-radius: 4px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }

  .scenario-desc {
    font-size: 13px;
    color: var(--muted);
    line-height: 1.4;
    margin: 0 0 8px;
  }

  .scenario-meta {
    font-size: 11px;
    color: var(--muted);
  }

  .scenario-candidates {
    font-variant-numeric: tabular-nums;
  }

  .scenario-run-btn {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 16px;
    font-size: 12px;
    border-radius: 8px;
    background: transparent;
    border: 1px solid var(--ring);
    color: var(--ink);
    cursor: pointer;
    font-weight: 600;
  }

  .scenario-run-btn:not(:disabled):hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .scenario-run-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Pass/fail summary */
  .testbed-summary {
    display: flex;
    align-items: baseline;
    gap: 12px;
    padding: 20px 24px;
    border-radius: 14px;
    margin-top: 12px;
  }

  .testbed-summary.summary-pass {
    background: rgba(34, 197, 94, 0.1);
    border: 1px solid rgba(34, 197, 94, 0.3);
  }

  .testbed-summary.summary-fail {
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
  }

  .testbed-summary-count {
    font-size: 36px;
    font-weight: 800;
    font-variant-numeric: tabular-nums;
  }

  .summary-pass .testbed-summary-count {
    color: var(--success);
  }

  .summary-fail .testbed-summary-count {
    color: var(--danger, #ef4444);
  }

  .testbed-summary-label {
    font-size: 18px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 1px;
  }

  .summary-pass .testbed-summary-label {
    color: var(--success);
  }

  .summary-fail .testbed-summary-label {
    color: var(--danger, #ef4444);
  }

  .match-icon {
    font-size: 16px;
    font-weight: 700;
  }

  .match-icon.pass {
    color: var(--success);
  }

  .match-icon.fail {
    color: var(--danger, #ef4444);
  }

  .reason-cell {
    font-size: 12px;
    color: var(--muted);
    max-width: 240px;
    line-height: 1.4;
  }

  /* Run All grid */
  .runall-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: 8px;
  }

  .runall-cell {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 10px 14px;
    border-radius: 10px;
    border: 1px solid var(--ring);
    background: var(--card);
  }

  .runall-cell.runall-pass {
    border-color: rgba(34, 197, 94, 0.4);
    background: rgba(34, 197, 94, 0.06);
  }

  .runall-cell.runall-fail {
    border-color: rgba(239, 68, 68, 0.4);
    background: rgba(239, 68, 68, 0.06);
  }

  .runall-name {
    font-size: 12px;
    font-weight: 600;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }

  .runall-score {
    font-size: 13px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
    margin-left: 8px;
  }

  .runall-pass .runall-score {
    color: var(--success);
  }

  .runall-fail .runall-score {
    color: var(--danger, #ef4444);
  }

  .runall-pending .runall-score {
    color: var(--muted);
  }

  /* Responsive */
  @media (max-width: 900px) {
    .playground-layout {
      flex-direction: column;
    }

    .input-panel {
      max-width: none;
      border-right: none;
      border-bottom: 1px solid var(--ring);
    }

    .output-panel {
      min-height: 300px;
    }

    .tab-bar {
      padding: 0 12px;
    }
  }

  .verdicts-grid {
    display: grid;
    gap: 12px;
  }

  .verdict-confidence {
    font-size: 12px;
    color: var(--muted);
    margin-left: auto;
  }

  .review-notes {
    padding: 12px 16px;
    background: var(--bg-2);
    border-radius: 10px;
    font-size: 13px;
    color: var(--ink);
    margin-bottom: 12px;
    border-left: 3px solid var(--accent);
  }

  /* ── 3-bucket diff (agreed / vetoed / promoted) ── */
  .notes-banner {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-left: 4px solid var(--accent);
    border-radius: 10px;
    padding: 12px 16px;
    margin-bottom: 16px;
  }

  .notes-banner.notes-warning {
    border-left-color: var(--warning, #f97316);
    background: rgba(249, 115, 22, 0.06);
  }

  .notes-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    font-weight: 600;
    margin-bottom: 4px;
  }

  .notes-text {
    margin: 0;
    font-size: 13px;
    line-height: 1.55;
    color: var(--ink);
    white-space: pre-wrap;
  }

  .bucket-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 14px;
  }

  @media (max-width: 1080px) {
    .bucket-grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  .bucket-col {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    min-height: 80px;
  }

  .bucket-agreed {
    border-top: 3px solid var(--success, #22c55e);
  }
  .bucket-vetoed {
    border-top: 3px solid var(--warning, #f97316);
  }
  .bucket-promoted {
    border-top: 3px solid #3b82f6;
  }

  .bucket-header {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 10px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--ring);
  }

  .bucket-title {
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--ink);
  }

  .bucket-count {
    font-size: 13px;
    font-variant-numeric: tabular-nums;
    color: var(--muted);
    font-weight: 600;
  }

  .bucket-empty {
    color: var(--muted);
    font-size: 13px;
    text-align: center;
    padding: 16px 0;
  }

  .bucket-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .bucket-card {
    background: var(--bg-2, rgba(0, 0, 0, 0.12));
    border: 1px solid var(--ring);
    border-radius: 8px;
    padding: 10px 12px;
  }

  .bucket-card-name {
    font-size: 12px;
    font-weight: 600;
    color: var(--ink);
    word-break: break-all;
    margin-bottom: 4px;
  }

  .bucket-card-reason {
    font-size: 12px;
    color: var(--muted);
    line-height: 1.45;
    margin-bottom: 6px;
  }

  .link-btn.small {
    background: transparent;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .link-btn.small:hover {
    color: var(--ink);
  }

  /* ── Custom JSON tab ──────────────────────────────────────────── */
  .custom-container {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow: hidden;
    padding: 20px 24px;
    gap: 16px;
  }
  .custom-header h3 {
    margin: 0 0 8px 0;
  }
  .custom-hint {
    margin: 0;
    color: var(--muted);
    font-size: 12px;
    line-height: 1.5;
    max-width: 720px;
  }
  .custom-hint code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    color: var(--ink);
  }
  .custom-body {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 20px;
    overflow: hidden;
  }
  @media (max-width: 1080px) {
    .custom-body {
      grid-template-columns: minmax(0, 1fr);
      overflow-y: auto;
    }
  }
  .custom-input-col,
  .custom-result-col {
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
  .custom-textarea {
    flex: 1;
    min-height: 360px;
    width: 100%;
    padding: 14px;
    background: var(--bg-2, rgba(0, 0, 0, 0.15));
    border: 1px solid var(--ring);
    border-radius: 10px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12px;
    line-height: 1.5;
    color: var(--ink);
    resize: vertical;
  }
  .custom-actions {
    display: flex;
    gap: 10px;
    margin-top: 12px;
  }
  .custom-empty {
    padding: 40px 24px;
    text-align: center;
    color: var(--muted);
    font-size: 13px;
    border: 1px dashed var(--ring);
    border-radius: 12px;
  }

  /* ── Stats badge (token / latency / candidate count) ─────────── */
  .stats-badge {
    display: flex;
    flex-wrap: wrap;
    gap: 14px;
    padding: 10px 14px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 10px;
    font-size: 12px;
    color: var(--muted);
    margin-bottom: 12px;
    font-variant-numeric: tabular-nums;
  }
  .stats-badge .stat {
    display: inline-flex;
    align-items: baseline;
    gap: 4px;
  }
  .stats-badge .stat-label {
    text-transform: uppercase;
    letter-spacing: 0.4px;
    font-size: 10px;
  }
  .stats-badge .stat-value {
    color: var(--ink);
    font-weight: 700;
    font-size: 13px;
  }
  .stats-badge .stat-warning .stat-value,
  .stats-badge .stat-warning .stat-label {
    color: var(--warning, #f97316);
  }

  /* ── Expected-overlay markers (testbed) ────────────────────────── */
  .exp-marker {
    display: inline-block;
    width: 14px;
    text-align: center;
    font-weight: 700;
    margin-right: 4px;
  }
  .exp-marker.pass {
    color: var(--success, #22c55e);
  }
  .exp-marker.fail {
    color: var(--danger, #ef4444);
  }
  .bucket-card.expected-pass {
    border-left: 3px solid var(--success, #22c55e);
  }
  .bucket-card.expected-fail {
    border-left: 3px solid var(--danger, #ef4444);
  }

  /* ── Debug panels ──────────────────────────────────────────────── */
  .debug-panels {
    margin-top: 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .debug-warning {
    padding: 10px 14px;
    background: rgba(249, 115, 22, 0.08);
    border: 1px solid rgba(249, 115, 22, 0.4);
    border-radius: 8px;
    color: var(--warning, #f97316);
    font-size: 12px;
    font-weight: 600;
  }
  .debug-details {
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--card);
    overflow: hidden;
  }
  .debug-details summary {
    padding: 8px 14px;
    cursor: pointer;
    font-size: 12px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.4px;
    font-weight: 600;
    user-select: none;
  }
  .debug-details summary:hover {
    color: var(--ink);
  }
  .debug-details[open] summary {
    border-bottom: 1px solid var(--ring);
  }
  .debug-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  .debug-table th,
  .debug-table td {
    padding: 6px 12px;
    text-align: left;
    border-bottom: 1px solid var(--ring);
  }
  .debug-table th {
    color: var(--muted);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.4px;
    font-weight: 600;
  }
  .debug-table td.mono {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-variant-numeric: tabular-nums;
    width: 50px;
  }

  /* ── Four-bucket scoring breakdown (testbed) ───────────────────── */
  .scoring-breakdown {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-top: 14px;
  }
  .scoring-cell {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 10px 16px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 10px;
    min-width: 84px;
  }
  .scoring-num {
    font-size: 22px;
    font-weight: 700;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .scoring-label {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.4px;
    color: var(--muted);
    font-weight: 600;
  }
  .scoring-agreed {
    border-top: 3px solid var(--success, #22c55e);
  }
  .scoring-promoted {
    border-top: 3px solid #3b82f6;
  }
  .scoring-vetoed {
    border-top: 3px solid var(--warning, #f97316);
  }
  .scoring-silent {
    border-top: 3px solid var(--muted);
  }
  .scoring-wrong {
    border-top: 3px solid var(--danger, #ef4444);
    background: rgba(239, 68, 68, 0.06);
  }
  .scoring-wrong .scoring-num {
    color: var(--danger, #ef4444);
  }

  .wrong-list {
    margin-top: 14px;
    padding: 12px 14px;
    background: rgba(239, 68, 68, 0.04);
    border: 1px solid rgba(239, 68, 68, 0.25);
    border-radius: 10px;
  }
  .wrong-list-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--danger, #ef4444);
    font-weight: 700;
    margin-bottom: 8px;
  }
  .wrong-row {
    display: flex;
    align-items: baseline;
    gap: 10px;
    padding: 6px 0;
    font-size: 12px;
    border-top: 1px solid rgba(239, 68, 68, 0.15);
  }
  .wrong-row:first-of-type {
    border-top: none;
  }
  .wrong-tag {
    text-transform: uppercase;
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.4px;
    padding: 2px 6px;
    border-radius: 4px;
    background: rgba(239, 68, 68, 0.15);
    color: var(--danger, #ef4444);
  }
  .wrong-name {
    flex: 1;
    color: var(--ink);
    word-break: break-all;
  }
  .wrong-meta {
    color: var(--muted);
    font-size: 11px;
  }

  .timing-bar {
    display: flex;
    gap: 16px;
    font-size: 12px;
    color: var(--muted);
    padding: 8px 0;
  }

  .session-link {
    color: var(--accent);
    text-decoration: none;
  }

  .prompt-details {
    border: 1px solid var(--ring);
    border-radius: 10px;
    overflow: hidden;
  }

  .prompt-details summary {
    padding: 10px 16px;
    cursor: pointer;
    font-size: 13px;
    color: var(--muted);
    background: var(--bg-2);
  }

  .inline-prompt-editor {
    padding: 16px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    margin-bottom: 16px;
  }

  .prompt-editor-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
    gap: 12px;
  }

  .prompt-textarea {
    width: 100%;
    min-height: 120px;
    padding: 12px;
    font-family: monospace;
    font-size: 12px;
    line-height: 1.5;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-1);
    color: var(--ink);
    resize: vertical;
  }
</style>
