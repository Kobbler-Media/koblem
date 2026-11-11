<script lang="ts">
  import { onMount } from 'svelte';
  import {
    getSettings,
    updateSettings,
    testLibraryProvider,
    testReleaseProvider,
    testGpuService,
    getGpuLogsStats,
    clearGpuLogs,
    getHealth,
    listApiKeys,
    createApiKey,
    revokeApiKey
  } from '#lib/api/client.ts';
  import type {
    GpuLogsStats,
    ApiKeySummary,
    GpuServiceKind as ApiGpuServiceKind
  } from '#lib/api/client.ts';
  import type { SamplingConfig } from '#lib/types.ts';
  import SamplingControls, {
    applyPresetValues,
    emptySampling,
    fromSamplingConfig,
    toSamplingConfig,
    type SamplingState
  } from '#lib/components/SamplingControls.svelte';
  import type { SamplingPreset } from '#lib/types.ts';

  /** Settings-table keys + labels for the per-surface sampling defaults block.
   *
   * 🔴 Mirrors `SAMPLING_KEYS` in `api/src/sampling.rs` — that list is what the
   * server actually reads. A key here with no reader is a knob that does nothing.
   *
   * Every panel is **inherit by default**: an empty field is not sent at all, so
   * the call site's own value (or llama-server's flag default) stands. The six
   * that ship with real values instead — the three music writers and the three
   * prompt assists — say so in their descriptions. */
  const SAMPLING_KEYS: { key: string; label: string; desc: string; group: string }[] = [
    {
      group: 'Release pipeline',
      key: 'sampling_review_candidates',
      label: 'Review candidates (/api/v1/review-candidates)',
      desc: 'Applies to every kobbler/kobload review call — the main "release fondling" path.'
    },
    {
      group: 'Release pipeline',
      key: 'sampling_filter_releases',
      label: 'Filter releases (/filter-releases)',
      desc: 'RSS prefilter pass — cheap semantic match before the full review.'
    },
    {
      group: 'Release pipeline',
      key: 'sampling_validate_release',
      label: 'Validate release (/validate-release)',
      desc: 'Single-release validity check.'
    },
    {
      group: 'Release pipeline',
      key: 'sampling_rank_releases',
      label: 'Rank releases (/rank-releases)',
      desc: 'Ranks releases by quality signal.'
    },
    {
      group: 'Conversation',
      key: 'sampling_chat_default',
      label: 'Chat (/api/v1/chat)',
      desc: 'Default for the internal chat endpoint when the UI does not override.'
    },
    {
      group: 'Conversation',
      key: 'sampling_completions_default',
      label: 'Chat completions (/v1/chat/completions proxy)',
      desc: 'Default for OpenAI-compatible proxy calls from external clients.'
    },
    {
      group: 'Conversation',
      key: 'sampling_agent',
      label: 'Agent loop',
      desc:
        'The tool-calling agent, every round of it. Separate from the chat box on purpose: ' +
        'this one has to emit exact tool JSON, so loosening chat for prose should not loosen it.'
    },
    {
      group: 'Video prompt writers',
      key: 'sampling_director',
      label: 'Director',
      desc:
        'Every LLM call the video Director makes — writing a script, reworking shots, ' +
        'continuing a timeline, titling, reviewing, and the chat inside the Director. ' +
        'Each call site picks its own temperature (0.3 for tidy edits up to 0.8 for new ' +
        'material); setting one here overrides ALL of them with the same value.'
    },
    {
      group: 'Video prompt writers',
      key: 'sampling_ltx_video',
      label: 'LTX / H3 prompt writer',
      desc: 'Turns a short idea (plus reference stills) into the long video prompts LTX and H3 want.'
    },
    {
      group: 'Video prompt writers',
      key: 'sampling_ltx_critic',
      label: 'Clip critic',
      desc:
        'Scores rendered frames against the prompt. A vision call, and a judging one — ' +
        'it ships at temperature 0.2 for a reason.'
    },
    {
      group: 'Music & lyrics',
      key: 'sampling_music',
      label: 'Music writer (captions + lyrics)',
      desc:
        '🔴 The one surface that is NOT inherit-by-default: it ships creative ' +
        '(temp 1.0, top_p 0.95, top_k 20, min_p 0, presence_penalty 0) because it writes ' +
        'captions and lyrics, and a presence penalty fights a chorus, which repeats on purpose. ' +
        "Clearing every field here restores those defaults rather than llama-server's. " +
        'Also the shared knob: Music 3 and SongGen inherit from it unless given their own values.'
    },
    {
      group: 'Music & lyrics',
      key: 'sampling_music3',
      label: 'Music 3 writer',
      desc:
        'Ships with the same creative defaults as the music writer above and inherits ' +
        'whatever you set there; fill fields here only when MiniMax Music 3 needs to differ. ' +
        '⚠️ Leave Thinking off — with thinking on, this path has burned its whole token ' +
        'budget on a reasoning trace and returned an empty caption.'
    },
    {
      group: 'Music & lyrics',
      key: 'sampling_songgen',
      label: 'SongGen writer',
      desc:
        'Ships with the same creative defaults as the music writer above and inherits whatever ' +
        'you set there; fill fields here only when SongGen needs to differ.'
    },
    {
      group: 'Image & character assists',
      key: 'sampling_images',
      label: 'FLUX.2 prompt assist',
      desc:
        'The "suggest a prompt" button on the image page. Ships at temp 0.85 / top_p 0.95 — ' +
        'the values it forced before this panel existed.'
    },
    {
      group: 'Image & character assists',
      key: 'sampling_krea2',
      label: 'Krea 2 prompt assist',
      desc:
        "Same button for Krea 2, deliberately more faithful than FLUX.2's: it ships at " +
        "temp 0.4 / top_p 0.9 because it rewrites the user's prompt rather than inventing one."
    },
    {
      group: 'Image & character assists',
      key: 'sampling_character_sheet',
      label: 'Character sheet',
      desc:
        'The one-shot call that turns a description into the structured character sheet ' +
        '(schema-constrained JSON). Raising temperature here mostly buys you weirder rigs.'
    },
    {
      group: 'Housekeeping',
      key: 'sampling_title',
      label: 'Generation titles',
      desc: 'The tiny background call that names a generation from its inputs. 24 tokens, no drama.'
    }
  ];

  let libraryProviderUrl = $state('');
  let libraryProviderToken = $state('');
  let releaseProviderUrl = $state('');
  let releaseProviderToken = $state('');
  let saving = $state(false);
  let testing = $state(false);
  let testingRelease = $state(false);
  let saveMessage = $state('');
  let testMessage = $state('');
  let testOk = $state(false);
  let releaseTestMessage = $state('');
  let releaseTestOk = $state(false);
  let loading = $state(true);

  // GPU services
  let gpuLockEnabled = $state(true);
  let gpuTtsHoldSeconds = $state(30);
  let llamaServerUrl = $state('');
  let visionServiceUrl = $state('');
  let textEmbedServiceUrl = $state('');
  let ttsBreezeServiceUrl = $state('');
  let sttServiceUrl = $state('');
  let aceServerUrl = $state('');
  let sa3ServerUrl = $state('');
  // MiniMax Music 3 — the `music3` plugin on comfy-api, so this is normally the SAME host as
  // minimax_h3 (and ltx25). It is its own setting so it can be split onto another box later.
  let music3ServerUrl = $state('');
  let songgenServerUrl = $state('');
  let flux2ServerUrl = $state('');
  let krea2ServerUrl = $state('');
  let ltx25ServerUrl = $state('');
  let minimaxH3ServerUrl = $state('');
  let mattingServiceUrl = $state('');
  let spar3dServiceUrl = $state('');
  let pixal3dServerUrl = $state('');

  /** The services this page renders a row for. Narrowed from the client's union rather than
   *  re-listed, so a name the API would reject collapses to `never` and fails the Records below. */
  type GpuServiceKind = Extract<
    ApiGpuServiceKind,
    | 'llm'
    | 'vision'
    | 'text_embed'
    | 'tts'
    | 'stt'
    | 'music'
    | 'sa3'
    | 'music3'
    | 'songgen'
    | 'flux2'
    | 'krea2'
    | 'ltx'
    | 'minimax_h3'
    | 'matting'
    | 'spar3d'
    | 'pixal3d'
  >;
  type ServiceTest = { loading: boolean; ok: boolean; message: string };
  const initialTest = (): ServiceTest => ({
    loading: false,
    ok: false,
    message: ''
  });
  let serviceTests = $state<Record<GpuServiceKind, ServiceTest>>({
    llm: initialTest(),
    vision: initialTest(),
    text_embed: initialTest(),
    tts: initialTest(),
    stt: initialTest(),
    music: initialTest(),
    sa3: initialTest(),
    music3: initialTest(),
    songgen: initialTest(),
    flux2: initialTest(),
    krea2: initialTest(),
    ltx: initialTest(),
    minimax_h3: initialTest(),
    matting: initialTest(),
    spar3d: initialTest(),
    pixal3d: initialTest()
  });
  // Only the core engines report a model name via /health; the rest render an
  // empty label until a Test surfaces one in its result message.
  let serviceModels = $state<Record<GpuServiceKind, string>>({
    llm: '',
    vision: '',
    text_embed: '',
    tts: '',
    stt: '',
    music: '',
    sa3: '',
    music3: '',
    songgen: '',
    flux2: '',
    krea2: '',
    ltx: '',
    minimax_h3: '',
    matting: '',
    spar3d: '',
    pixal3d: ''
  });

  // Retention
  let retentionMaxEntries = $state(1000);
  let retentionMaxAgeDays = $state(7);
  let retentionMaxSampleMb = $state(1024);
  let saveSampleDefault = $state(true);
  let gpuStats = $state<GpuLogsStats | null>(null);
  let clearingLogs = $state(false);

  // Queue. Empty webhook URL = no webhook; 0 minutes/days = that sweep off.
  let queueWebhookUrl = $state('');
  let queueStuckAlertMinutes = $state(30);
  let queueRowRetentionDays = $state(14);
  let queueFilesRetentionDays = $state(3);

  // Generation gallery. Empty dir = persistence DISABLED for
  // images/matting/tts/stt (music uses its own dir regardless).
  // Retention 0 = keep until manually deleted.
  let generationsDir = $state('');
  let genRetentionMaxEntries = $state(0);
  let genRetentionMaxAgeDays = $state(0);

  // API keys for external (LAN) callers — the `kob` CLI / scripts. CRUD against
  // /api/v1/auth/api-keys; independent of the bulk "Save All Settings" below.
  let apiKeys = $state<ApiKeySummary[]>([]);
  let newKeyName = $state('');
  let creatingKey = $state(false);
  let apiKeyError = $state('');
  let revealedKeys = $state<Record<string, boolean>>({});
  let copiedKeyId = $state('');

  // Sampling defaults per route. The object is keyed by the settings-table key.
  let samplingStates: Record<string, SamplingState> = $state(
    Object.fromEntries(SAMPLING_KEYS.map(({ key }) => [key, emptySampling()]))
  );

  async function refreshGpuStats() {
    try {
      gpuStats = await getGpuLogsStats();
    } catch {
      gpuStats = null;
    }
  }

  onMount(async () => {
    try {
      const settings = await getSettings();
      libraryProviderUrl = (settings.library_provider_url as string) || '';
      libraryProviderToken = (settings.library_provider_token as string) || '';
      releaseProviderUrl = (settings.release_provider_url as string) || '';
      releaseProviderToken = (settings.release_provider_token as string) || '';
      gpuLockEnabled =
        settings.gpu_lock_enabled == null ? true : Boolean(settings.gpu_lock_enabled);
      gpuTtsHoldSeconds = Number(settings.gpu_tts_hold_seconds) || 30;
      llamaServerUrl = (settings.llama_server_url as string) || '';
      visionServiceUrl = (settings.vision_service_url as string) || '';
      textEmbedServiceUrl = (settings.text_embed_service_url as string) || '';
      ttsBreezeServiceUrl = (settings.tts_breeze_service_url as string) || '';
      sttServiceUrl = (settings.stt_service_url as string) || '';
      aceServerUrl = (settings.ace_server_url as string) || '';
      sa3ServerUrl = (settings.sa3_server_url as string) || '';
      music3ServerUrl = (settings.music3_server_url as string) || '';
      songgenServerUrl = (settings.songgen_server_url as string) || '';
      flux2ServerUrl = (settings.flux2_server_url as string) || '';
      krea2ServerUrl = (settings.krea2_server_url as string) || '';
      ltx25ServerUrl = (settings.ltx25_server_url as string) || '';
      minimaxH3ServerUrl = (settings.minimax_h3_server_url as string) || '';
      mattingServiceUrl = (settings.matting_service_url as string) || '';
      spar3dServiceUrl = (settings.spar3d_service_url as string) || '';
      pixal3dServerUrl = (settings.pixal3d_server_url as string) || '';
      retentionMaxEntries = Number(settings.gpu_log_retention_max_entries) || 1000;
      retentionMaxAgeDays = Number(settings.gpu_log_retention_max_age_days) || 7;
      retentionMaxSampleMb = Math.round(
        (Number(settings.gpu_sample_retention_max_bytes) || 1024 * 1024 * 1024) / (1024 * 1024)
      );
      saveSampleDefault =
        settings.gpu_sample_save_default == null ? true : Boolean(settings.gpu_sample_save_default);
      queueWebhookUrl = (settings.queue_webhook_url as string) || '';
      queueStuckAlertMinutes =
        settings.queue_stuck_alert_minutes == null
          ? 30
          : Number(settings.queue_stuck_alert_minutes);
      queueRowRetentionDays =
        settings.queue_row_retention_days == null ? 14 : Number(settings.queue_row_retention_days);
      queueFilesRetentionDays =
        settings.queue_files_retention_days == null
          ? 3
          : Number(settings.queue_files_retention_days);
      generationsDir = (settings.generations_dir as string) || '';
      genRetentionMaxEntries = Number(settings.generations_retention_max_entries) || 0;
      genRetentionMaxAgeDays = Number(settings.generations_retention_max_age_days) || 0;
      for (const { key } of SAMPLING_KEYS) {
        const raw = settings[key];
        samplingStates[key] = fromSamplingConfig(
          raw && typeof raw === 'object' ? (raw as SamplingConfig) : undefined
        );
      }
    } catch {
      // Settings not yet configured
    }
    await refreshGpuStats();
    await refreshApiKeys();
    try {
      const h = await getHealth();
      serviceModels = {
        ...serviceModels,
        llm: h.llama_model || '',
        vision: h.vision_model || '',
        tts: h.tts_model || '',
        stt: h.stt_model || ''
      };
    } catch {
      // health best-effort
    }
    loading = false;
  });

  async function handleClearLogs() {
    if (!confirm('Delete ALL gpu_logs entries and saved samples?')) return;
    clearingLogs = true;
    try {
      await clearGpuLogs();
      await refreshGpuStats();
    } finally {
      clearingLogs = false;
    }
  }

  async function refreshApiKeys() {
    try {
      apiKeys = await listApiKeys();
    } catch {
      apiKeys = [];
    }
  }

  async function handleCreateKey() {
    const name = newKeyName.trim();
    if (!name) {
      apiKeyError = 'Give the key a name first';
      return;
    }
    creatingKey = true;
    apiKeyError = '';
    try {
      const { id } = await createApiKey(name);
      newKeyName = '';
      await refreshApiKeys();
      // Reveal + copy the freshly minted key so it can be pasted into `kob config`.
      revealedKeys = { ...revealedKeys, [id]: true };
      const created = apiKeys.find((k) => k.id === id);
      if (created?.key) await copyKey(created.key, id);
    } catch (e) {
      apiKeyError = e instanceof Error ? e.message : 'Failed to create key';
    }
    creatingKey = false;
  }

  async function handleRevokeKey(id: string, name: string) {
    if (!confirm(`Revoke API key "${name}"? Callers using it will start getting 401s.`)) return;
    apiKeyError = '';
    try {
      await revokeApiKey(id);
      await refreshApiKeys();
    } catch (e) {
      apiKeyError = e instanceof Error ? e.message : 'Failed to revoke key';
    }
  }

  async function copyKey(value: string, id: string) {
    try {
      await navigator.clipboard.writeText(value);
      copiedKeyId = id;
      setTimeout(() => {
        if (copiedKeyId === id) copiedKeyId = '';
      }, 1500);
    } catch {
      // clipboard unavailable (insecure context) — reveal so it can be selected.
      revealedKeys = { ...revealedKeys, [id]: true };
    }
  }

  function maskKey(key: string): string {
    if (key.length <= 10) return '••••••';
    return `${key.slice(0, 4)}…${key.slice(-4)}`;
  }

  function fmtBytes(bytes: number): string {
    if (!bytes) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0;
    let v = bytes;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i += 1;
    }
    return `${v.toFixed(v < 10 ? 2 : 1)} ${units[i]}`;
  }

  async function handleSave() {
    saving = true;
    saveMessage = '';
    try {
      const payload: Record<string, unknown> = {
        library_provider_url: libraryProviderUrl,
        library_provider_token: libraryProviderToken,
        release_provider_url: releaseProviderUrl,
        release_provider_token: releaseProviderToken,
        gpu_lock_enabled: gpuLockEnabled,
        gpu_tts_hold_seconds: gpuTtsHoldSeconds,
        llama_server_url: llamaServerUrl,
        vision_service_url: visionServiceUrl,
        text_embed_service_url: textEmbedServiceUrl,
        tts_breeze_service_url: ttsBreezeServiceUrl,
        stt_service_url: sttServiceUrl,
        ace_server_url: aceServerUrl,
        sa3_server_url: sa3ServerUrl,
        music3_server_url: music3ServerUrl,
        songgen_server_url: songgenServerUrl,
        flux2_server_url: flux2ServerUrl,
        krea2_server_url: krea2ServerUrl,
        ltx25_server_url: ltx25ServerUrl,
        minimax_h3_server_url: minimaxH3ServerUrl,
        matting_service_url: mattingServiceUrl,
        spar3d_service_url: spar3dServiceUrl,
        pixal3d_server_url: pixal3dServerUrl,
        gpu_log_retention_max_entries: retentionMaxEntries,
        gpu_log_retention_max_age_days: retentionMaxAgeDays,
        gpu_sample_retention_max_bytes: retentionMaxSampleMb * 1024 * 1024,
        gpu_sample_save_default: saveSampleDefault,
        queue_webhook_url: queueWebhookUrl.trim(),
        queue_stuck_alert_minutes: queueStuckAlertMinutes,
        queue_row_retention_days: queueRowRetentionDays,
        queue_files_retention_days: queueFilesRetentionDays,
        generations_dir: generationsDir.trim(),
        generations_retention_max_entries: genRetentionMaxEntries,
        generations_retention_max_age_days: genRetentionMaxAgeDays
      };
      // Persist each sampling panel as its own JSONB blob. Empty (all-null)
      // state writes `null` — the backend treats missing/null rows as
      // "use the hardcoded Deterministic baseline", so this doubles as reset.
      for (const { key } of SAMPLING_KEYS) {
        const cfg = toSamplingConfig(samplingStates[key]);
        payload[key] = Object.keys(cfg).length === 0 ? null : cfg;
      }
      await updateSettings(payload);
      saveMessage = 'Settings saved';
      await refreshGpuStats();
    } catch (e) {
      saveMessage = e instanceof Error ? e.message : 'Failed to save';
    }
    saving = false;
  }

  function resetSampling(key: string) {
    samplingStates[key] = emptySampling();
  }

  /// Bulk-apply: set every per-surface sampling panel to the same preset in
  /// one click. Every field (including the preset values) is written, so this
  /// overwrites whatever was there — intentional, since the per-panel toggles
  /// stay available for follow-up tweaks.
  let bulkPreset = $state<SamplingPreset | ''>('');
  let bulkMessage = $state('');
  function applyBulkPreset() {
    if (!bulkPreset) {
      bulkMessage = 'Pick a preset first';
      return;
    }
    const next: SamplingState = applyPresetValues({
      ...emptySampling(),
      preset: bulkPreset
    });
    for (const { key } of SAMPLING_KEYS) {
      samplingStates[key] = { ...next };
    }
    bulkMessage = `Applied ${bulkPreset} to all ${SAMPLING_KEYS.length} surfaces — don't forget to Save.`;
  }
  function clearAllSampling() {
    for (const { key } of SAMPLING_KEYS) {
      samplingStates[key] = emptySampling();
    }
    bulkMessage = `Cleared all ${SAMPLING_KEYS.length} surfaces — every one back to inherit.`;
  }

  const BULK_PRESET_OPTIONS: { id: SamplingPreset | ''; name: string }[] = [
    { id: '', name: '— pick a preset —' },
    { id: 'deterministic', name: 'Deterministic (temp=0)' },
    { id: 'qwen_instruct_general', name: 'Qwen Instruct General' },
    { id: 'qwen_instruct_reasoning', name: 'Qwen Instruct Reasoning' }
  ];

  async function handleTest() {
    testing = true;
    testMessage = '';
    testOk = false;
    try {
      const result = await testLibraryProvider(libraryProviderUrl, libraryProviderToken);
      testOk = result.success;
      testMessage =
        result.message || (result.success ? 'Connection successful' : 'Connection failed');
    } catch (e) {
      testOk = false;
      testMessage = e instanceof Error ? e.message : 'Connection failed';
    }
    testing = false;
  }

  async function handleTestRelease() {
    testingRelease = true;
    releaseTestMessage = '';
    releaseTestOk = false;
    try {
      const result = await testReleaseProvider(releaseProviderUrl, releaseProviderToken);
      releaseTestOk = result.success;
      releaseTestMessage =
        result.message || (result.success ? 'Connection successful' : 'Connection failed');
    } catch (e) {
      releaseTestOk = false;
      releaseTestMessage = e instanceof Error ? e.message : 'Connection failed';
    }
    testingRelease = false;
  }

  async function handleTestGpuService(service: GpuServiceKind, url: string) {
    serviceTests[service] = { loading: true, ok: false, message: '' };
    try {
      const result = await testGpuService(service, url);
      serviceTests[service] = {
        loading: false,
        ok: result.success,
        message: result.message
      };
    } catch (e) {
      serviceTests[service] = {
        loading: false,
        ok: false,
        message: e instanceof Error ? e.message : 'Connection failed'
      };
    }
  }

  const gpuServices: {
    key: GpuServiceKind;
    label: string;
    placeholder: string;
  }[] = [
    {
      key: 'llm',
      label: 'LLM (llama-server)',
      placeholder: 'http://llama-server:8080'
    },
    {
      key: 'vision',
      label: 'Embeddings: images (EmbeddingGemma-2)',
      placeholder: 'http://embed-vision:8080'
    },
    {
      key: 'text_embed',
      // The always-on CPU/GPU text instance. Text requests never wait on the GPU gate; when it is
      // down they fall back to the image server above.
      label: 'Embeddings: text (EmbeddingGemma-2)',
      placeholder: 'http://embed-text:8080'
    },
    { key: 'tts', label: 'TTS (BreezeTTS 2)', placeholder: 'http://tts-breeze:8203' },
    {
      key: 'stt',
      label: 'STT (Parakeet)',
      placeholder: 'http://stt-parakeet:8022'
    },
    {
      key: 'music',
      label: 'Music (ACE-Step)',
      placeholder: 'http://acestep:8088'
    },
    {
      key: 'sa3',
      label: 'Music (Stable Audio 3)',
      placeholder: 'http://sa3:8104'
    },
    {
      key: 'music3',
      label: 'Music (MiniMax Music 3)',
      placeholder: 'http://comfy-api:8190'
    },
    {
      key: 'songgen',
      label: 'SongGen (LeVo2)',
      placeholder: 'http://songgen:8097'
    },
    {
      key: 'flux2',
      label: 'Images (FLUX.2)',
      placeholder: 'http://flux2:8080'
    },
    {
      key: 'krea2',
      label: 'Images (Krea 2)',
      placeholder: 'http://krea2:8080'
    },
    {
      key: 'ltx',
      label: 'Video (LTX-2.5)',
      // The comfy-api shim, normally the SAME host as MiniMax-H3 and Music 3 — three plugins in
      // one container. Its own setting so it can be split onto another box later.
      placeholder: 'http://comfy-api:8190'
    },
    {
      key: 'minimax_h3',
      label: 'Video + audio (MiniMax-H3)',
      placeholder: 'http://minimax-h3:8080'
    },
    {
      key: 'matting',
      label: 'Bg Removal (RMBG-2)',
      placeholder: 'http://matting:8898'
    },
    {
      key: 'spar3d',
      label: '3D (SPAR3D + RigAnything)',
      placeholder: 'http://spar3d-riganything:8098'
    },
    {
      key: 'pixal3d',
      // longcat-rig. Its OWN entry beside spar3d rather than a replacement for it: the two solve
      // the same problem, share no implementation, and sit on separate budget lines — this one
      // refuses to start below 11 GB free and is pinned to the 3060.
      label: '3D (Pixal3D / longcat-rig)',
      placeholder: 'http://longcat-rig:8102'
    }
  ];
  function getUrl(kind: GpuServiceKind): string {
    switch (kind) {
      case 'llm':
        return llamaServerUrl;
      case 'vision':
        return visionServiceUrl;
      case 'text_embed':
        return textEmbedServiceUrl;
      case 'tts':
        return ttsBreezeServiceUrl;
      case 'stt':
        return sttServiceUrl;
      case 'music':
        return aceServerUrl;
      case 'sa3':
        return sa3ServerUrl;
      case 'music3':
        return music3ServerUrl;
      case 'songgen':
        return songgenServerUrl;
      case 'flux2':
        return flux2ServerUrl;
      case 'krea2':
        return krea2ServerUrl;
      case 'ltx':
        return ltx25ServerUrl;
      case 'minimax_h3':
        return minimaxH3ServerUrl;
      case 'matting':
        return mattingServiceUrl;
      case 'spar3d':
        return spar3dServiceUrl;
      case 'pixal3d':
        return pixal3dServerUrl;
    }
  }
  function setUrl(kind: GpuServiceKind, v: string) {
    switch (kind) {
      case 'llm':
        llamaServerUrl = v;
        break;
      case 'vision':
        visionServiceUrl = v;
        break;
      case 'text_embed':
        textEmbedServiceUrl = v;
        break;
      case 'tts':
        ttsBreezeServiceUrl = v;
        break;
      case 'stt':
        sttServiceUrl = v;
        break;
      case 'music':
        aceServerUrl = v;
        break;
      case 'sa3':
        sa3ServerUrl = v;
        break;
      case 'music3':
        music3ServerUrl = v;
        break;
      case 'songgen':
        songgenServerUrl = v;
        break;
      case 'flux2':
        flux2ServerUrl = v;
        break;
      case 'krea2':
        krea2ServerUrl = v;
        break;
      case 'ltx':
        ltx25ServerUrl = v;
        break;
      case 'minimax_h3':
        minimaxH3ServerUrl = v;
        break;
      case 'matting':
        mattingServiceUrl = v;
        break;
      case 'pixal3d':
        pixal3dServerUrl = v;
        break;
      case 'spar3d':
        spar3dServiceUrl = v;
        break;
    }
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Settings</h2>
  </div>

  {#if loading}
    <div class="loading-card">Loading settings...</div>
  {:else}
    <div class="settings-card">
      <h3>Library Provider</h3>
      <p class="desc">Connect to your Kobbler instance for show/episode metadata lookups.</p>

      <div class="form-grid">
        <label class="field">
          <span class="field-label">Provider URL</span>
          <input
            type="text"
            bind:value={libraryProviderUrl}
            placeholder="https://kobbler.example.com"
          />
        </label>
        <label class="field">
          <span class="field-label">Provider Token</span>
          <input type="password" bind:value={libraryProviderToken} placeholder="API token" />
        </label>
      </div>

      <div class="form-actions">
        {#if testMessage}
          <span class="test-result" class:ok={testOk} class:fail={!testOk}>
            {testMessage}
          </span>
        {/if}
        <button
          class="btn-sec"
          type="button"
          disabled={testing || !libraryProviderUrl}
          onclick={handleTest}
        >
          {testing ? 'Testing...' : 'Test Connection'}
        </button>
      </div>
    </div>

    <div class="settings-card">
      <h3>Release Provider</h3>
      <p class="desc">Connect to your release provider for searching and scoring releases.</p>

      <div class="form-grid">
        <label class="field">
          <span class="field-label">Provider URL</span>
          <input
            type="text"
            bind:value={releaseProviderUrl}
            placeholder="https://releases.example.com"
          />
        </label>
        <label class="field">
          <span class="field-label">Provider Token</span>
          <input type="password" bind:value={releaseProviderToken} placeholder="API token" />
        </label>
      </div>

      <div class="form-actions">
        {#if releaseTestMessage}
          <span class="test-result" class:ok={releaseTestOk} class:fail={!releaseTestOk}>
            {releaseTestMessage}
          </span>
        {/if}
        <button
          class="btn-sec"
          type="button"
          disabled={testingRelease || !releaseProviderUrl}
          onclick={handleTestRelease}
        >
          {testingRelease ? 'Testing...' : 'Test Connection'}
        </button>
      </div>
    </div>

    <div class="settings-card">
      <h3>API Keys</h3>
      <p class="desc">
        Bearer tokens for external (LAN) callers — the <code>kob</code> CLI or your own scripts.
        Send as
        <code>Authorization: Bearer &lt;key&gt;</code>. Create one here, copy it, then run
        <code>kob config set --token &lt;key&gt;</code>. Revoke instantly if a key leaks.
      </p>

      <div class="form-actions key-create">
        <input
          type="text"
          class="key-name-input"
          placeholder="Key name (e.g. laptop, render-script)"
          bind:value={newKeyName}
          onkeydown={(e) => e.key === 'Enter' && handleCreateKey()}
        />
        <button
          class="btn-sec"
          type="button"
          disabled={creatingKey || !newKeyName.trim()}
          onclick={handleCreateKey}
        >
          {creatingKey ? 'Creating…' : 'Create key'}
        </button>
      </div>
      {#if apiKeyError}
        <p class="test-result fail">{apiKeyError}</p>
      {/if}

      {#if apiKeys.length === 0}
        <p class="key-empty">No API keys yet.</p>
      {:else}
        <div class="key-list">
          {#each apiKeys as k (k.id)}
            <div class="key-row" class:revoked={k.revoked}>
              <div class="key-main">
                <span class="key-name">{k.name}</span>
                {#if k.revoked}<span class="key-tag revoked">revoked</span>{/if}
              </div>
              {#if k.key}
                <code class="key-value">{revealedKeys[k.id] ? k.key : maskKey(k.key)}</code>
              {:else}
                <code class="key-value muted">hidden</code>
              {/if}
              <div class="key-meta">
                <span>created {new Date(k.created_at).toLocaleDateString()}</span>
                <span>
                  {k.last_used_at
                    ? `last used ${new Date(k.last_used_at).toLocaleString()}`
                    : 'never used'}
                </span>
              </div>
              <div class="key-actions">
                {#if k.key && !k.revoked}
                  <button
                    class="link-btn"
                    type="button"
                    onclick={() =>
                      (revealedKeys = {
                        ...revealedKeys,
                        [k.id]: !revealedKeys[k.id]
                      })}>{revealedKeys[k.id] ? 'Hide' : 'Show'}</button
                  >
                  <button class="link-btn" type="button" onclick={() => copyKey(k.key!, k.id)}>
                    {copiedKeyId === k.id ? 'Copied!' : 'Copy'}
                  </button>
                {/if}
                {#if !k.revoked}
                  <button
                    class="link-btn danger"
                    type="button"
                    onclick={() => handleRevokeKey(k.id, k.name)}>Revoke</button
                  >
                {/if}
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <div class="settings-card">
      <h3>GPU Services</h3>
      <p class="desc">
        Koblem fronts these GPU engines and serialises the gated ones with a priority lock (TTS &gt;
        Vision &gt; STT). Turn the lock off if your GPU has enough VRAM to run everything
        concurrently. Each URL has a Test button that probes the engine's <code>/health</code>;
        leave a URL blank to disable that tab. Background removal is ungated.
      </p>

      <div class="check-row">
        <label class="checkbox">
          <input type="checkbox" bind:checked={gpuLockEnabled} />
          <span>GPU lock enabled</span>
        </label>
        <p class="check-desc">Serialise vision/TTS/STT so only one runs at a time.</p>
      </div>

      <label class="field">
        <span class="field-label">TTS hold after release (seconds)</span>
        <input type="number" min="0" max="600" bind:value={gpuTtsHoldSeconds} />
        <span class="field-help">
          After a TTS call finishes, keep the GPU reserved for another N seconds so
          sentence-by-sentence streaming doesn't lose the lock between calls. 0 disables.
        </span>
      </label>

      <div class="service-list">
        {#each gpuServices as svc (svc.key)}
          {@const test = serviceTests[svc.key]}
          {@const model = serviceModels[svc.key]}
          <div class="service-row">
            <div class="service-label">
              <span>{svc.label}</span>
              {#if model}<span class="service-model" title={model}>{model}</span>{/if}
            </div>
            <input
              type="text"
              class="service-url"
              value={getUrl(svc.key)}
              oninput={(e) => setUrl(svc.key, (e.target as HTMLInputElement).value)}
              placeholder={svc.placeholder}
            />
            <button
              class="btn-sec"
              type="button"
              disabled={test.loading || !getUrl(svc.key)}
              onclick={() => handleTestGpuService(svc.key, getUrl(svc.key))}
            >
              {test.loading ? 'Testing…' : 'Test'}
            </button>
            {#if test.message}
              <div class="service-status" class:ok={test.ok} class:fail={!test.ok}>
                {test.message}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    </div>

    <div class="settings-card">
      <h3>GPU Logs Retention</h3>
      <p class="desc">
        Rolling retention for the tier-2 playground history. Samples (saved audio/image files) live
        on disk; logs live in postgres. Sweeper runs hourly.
      </p>

      <div class="form-grid triple">
        <label class="field">
          <span class="field-label">Max entries</span>
          <input type="number" min="1" bind:value={retentionMaxEntries} />
        </label>
        <label class="field">
          <span class="field-label">Max age (days)</span>
          <input type="number" min="1" bind:value={retentionMaxAgeDays} />
        </label>
        <label class="field">
          <span class="field-label">Max total sample size (MB)</span>
          <input type="number" min="0" bind:value={retentionMaxSampleMb} />
        </label>
      </div>

      <div class="check-row">
        <label class="checkbox">
          <input type="checkbox" bind:checked={saveSampleDefault} />
          <span>Default "save sample" checkbox on in playground</span>
        </label>
        <p class="check-desc">Opt-in per-call when off; prechecked when on.</p>
      </div>

      {#if gpuStats}
        <div class="stats-readout">
          <div><span>Entries:</span> {gpuStats.total_entries}</div>
          <div><span>Saved samples:</span> {gpuStats.total_samples}</div>
          <div>
            <span>Sample disk:</span>
            {fmtBytes(gpuStats.total_sample_bytes)}
          </div>
          {#if gpuStats.oldest_created_at}
            <div><span>Oldest:</span> {gpuStats.oldest_created_at}</div>
          {/if}
          {#if gpuStats.newest_created_at}
            <div><span>Newest:</span> {gpuStats.newest_created_at}</div>
          {/if}
        </div>
      {/if}

      <div class="form-actions">
        <button
          class="btn-sec btn-danger"
          type="button"
          disabled={clearingLogs}
          onclick={handleClearLogs}
        >
          {clearingLogs ? 'Clearing…' : 'Clear all logs + samples'}
        </button>
      </div>
    </div>

    <div class="settings-card">
      <h3>Queue</h3>
      <p class="desc">
        The global GPU queue: what it tells you about an unattended batch, and how long it keeps the
        record. Row retention is rows only — a generated asset lives until you delete it.
      </p>

      <label class="field">
        <span class="field-label">Notification webhook URL</span>
        <input
          type="text"
          placeholder="https://ntfy.sh/my-topic (empty = no webhook)"
          bind:value={queueWebhookUrl}
        />
        <span class="field-help">
          One POST carrying a pre-rendered <code>text</code> field, so ntfy, gotify, Discord and Slack
          all render it without templating. Fires on batch completion and on individual failures — never
          on progress.
        </span>
      </label>

      <div class="form-grid">
        <label class="field">
          <span class="field-label">Alert when a job has waited (minutes, 0 = off)</span>
          <input type="number" min="0" bind:value={queueStuckAlertMinutes} />
        </label>
        <label class="field">
          <span class="field-label">Keep finished queue rows for (days, 0 = forever)</span>
          <input type="number" min="0" bind:value={queueRowRetentionDays} />
        </label>
        <label class="field">
          <span class="field-label">Keep queue working files for (days, 0 = forever)</span>
          <input type="number" min="0" bind:value={queueFilesRetentionDays} />
        </label>
      </div>
    </div>

    <div class="settings-card">
      <h3>Generation Gallery</h3>
      <p class="desc">
        Per-page history of generated assets (music, images, TTS, STT, background removal) with a
        delete button. Music always saves to its own dir.
        <strong
          >Images, background removal, TTS and STT save nothing until you set a directory below</strong
        > — this keeps a fresh deploy from writing into a volume before you've chosen where. Retention
        defaults to 0 = keep until you delete manually.
      </p>

      <label class="field">
        <span class="field-label">Output directory</span>
        <input
          type="text"
          placeholder="/var/lib/koblem/generations (empty = disabled)"
          bind:value={generationsDir}
        />
      </label>

      <div class="form-grid">
        <label class="field">
          <span class="field-label">Auto-prune: max per service (0 = off)</span>
          <input type="number" min="0" bind:value={genRetentionMaxEntries} />
        </label>
        <label class="field">
          <span class="field-label">Auto-prune: max age days (0 = off)</span>
          <input type="number" min="0" bind:value={genRetentionMaxAgeDays} />
        </label>
      </div>
    </div>

    <div class="settings-card">
      <h3>Sampling Defaults</h3>
      <p class="desc">
        Per-surface llama.cpp sampling defaults — one panel per place koblem talks to the LLM. An
        empty field is <strong>not sent at all</strong>: the call site's own value stands, and
        failing that llama-server's flags do. Callers can still override per request. Reset clears a
        panel back to that inherit behaviour (the music, FLUX.2 and Krea 2 panels fall back
        to the shipped values named in their descriptions instead).
      </p>

      <div class="bulk-apply">
        <div class="bulk-apply-label">
          Start from a preset
          <span class="bulk-apply-sub"
            >— applies to all {SAMPLING_KEYS.length} surfaces, tweak individually below</span
          >
        </div>
        <div class="bulk-apply-row">
          <select class="bulk-apply-select" bind:value={bulkPreset}>
            {#each BULK_PRESET_OPTIONS as opt (opt.id)}
              <option value={opt.id}>{opt.name}</option>
            {/each}
          </select>
          <button class="btn-sec" type="button" onclick={applyBulkPreset}>
            Apply to all surfaces
          </button>
          <button class="btn-sec" type="button" onclick={clearAllSampling}> Clear all </button>
          {#if bulkMessage}
            <span class="bulk-apply-msg">{bulkMessage}</span>
          {/if}
        </div>
      </div>

      <div class="sampling-panels">
        {#each SAMPLING_KEYS as panel, i (panel.key)}
          {#if i === 0 || SAMPLING_KEYS[i - 1].group !== panel.group}
            <div class="sampling-group">{panel.group}</div>
          {/if}
          <div class="sampling-panel">
            <div class="sampling-panel-header">
              <div>
                <div class="sampling-panel-title">{panel.label}</div>
                <div class="sampling-panel-desc">{panel.desc}</div>
              </div>
              <button class="link-btn" type="button" onclick={() => resetSampling(panel.key)}
                >Reset</button
              >
            </div>
            <SamplingControls
              bind:value={samplingStates[panel.key]}
              label="Customise"
              collapsible
              defaultExpanded={false}
            />
          </div>
        {/each}
      </div>
    </div>

    <div class="save-bar">
      {#if saveMessage}
        <span class="save-result">{saveMessage}</span>
      {/if}
      <button class="btn-primary" type="button" disabled={saving} onclick={handleSave}>
        {saving ? 'Saving...' : 'Save All Settings'}
      </button>
    </div>
  {/if}
</div>

<style>
  .sampling-group {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.6px;
    color: var(--muted);
    margin: 6px 0 -2px;
  }

  .settings-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 24px;
    max-width: 760px;
    margin-bottom: 16px;
    font: inherit;
  }

  .settings-card h3 {
    margin: 0 0 4px;
    font-size: 16px;
    font-weight: 600;
  }

  .bulk-apply {
    border: 1px dashed var(--ring);
    border-radius: 10px;
    padding: 12px 14px;
    margin-bottom: 14px;
    background: rgba(0, 0, 0, 0.04);
  }
  :global([data-theme='light']) .bulk-apply {
    background: rgba(0, 0, 0, 0.015);
  }
  .bulk-apply-label {
    font-size: 12px;
    font-weight: 600;
    margin-bottom: 8px;
  }
  .bulk-apply-sub {
    font-weight: 400;
    color: var(--muted);
  }
  .bulk-apply-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .bulk-apply-select {
    padding: 6px 8px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: transparent;
    color: var(--ink);
    font-size: 13px;
    min-width: 240px;
  }
  .bulk-apply-msg {
    font-size: 12px;
    color: var(--muted);
    margin-left: 4px;
  }

  .sampling-panels {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .sampling-panel {
    border: 1px solid var(--ring);
    border-radius: 10px;
    padding: 12px 14px;
  }

  .sampling-panel-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 12px;
    margin-bottom: 8px;
  }

  .sampling-panel-title {
    font-weight: 600;
    font-size: 13px;
  }

  .sampling-panel-desc {
    font-size: 12px;
    color: var(--muted);
    margin-top: 2px;
  }

  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    padding: 0;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .link-btn:hover {
    color: var(--accent);
  }

  .settings-card .desc {
    margin: 0 0 16px;
    font-size: 13px;
    color: var(--muted);
    line-height: 1.5;
  }

  .form-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }
  .form-grid.triple {
    grid-template-columns: repeat(3, 1fr);
  }
  @media (max-width: 600px) {
    .form-grid,
    .form-grid.triple {
      grid-template-columns: 1fr;
    }
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 13px;
  }
  .field-label {
    font-size: 12px;
    font-weight: 500;
    color: var(--muted);
  }
  .field-help {
    font-size: 12px;
    color: var(--muted);
    line-height: 1.45;
  }
  .field input {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
  }

  .check-row {
    margin: 12px 0;
  }
  .checkbox {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    font-weight: 500;
    color: var(--ink);
  }
  .checkbox input {
    margin: 0;
  }
  .check-desc {
    margin: 4px 0 0 24px;
    font-size: 12px;
    color: var(--muted);
    line-height: 1.45;
  }

  .service-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 16px;
  }
  .service-row {
    display: grid;
    grid-template-columns: 160px 1fr auto;
    gap: 10px;
    align-items: center;
  }
  .service-label {
    font-size: 13px;
    font-weight: 500;
    color: var(--ink);
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .service-model {
    font-size: 11px;
    font-family: monospace;
    font-weight: 400;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .service-url {
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
    box-sizing: border-box;
  }
  .service-status {
    grid-column: 2 / -1;
    font-size: 12px;
    line-height: 1.45;
  }
  .service-status.ok {
    color: var(--success);
  }
  .service-status.fail {
    color: var(--danger);
  }
  @media (max-width: 600px) {
    .service-row {
      grid-template-columns: 1fr;
    }
    .service-status {
      grid-column: 1;
    }
  }

  .form-actions {
    display: flex;
    gap: 8px;
    align-items: center;
    justify-content: flex-end;
    margin-top: 16px;
    flex-wrap: wrap;
  }

  .test-result {
    font-size: 13px;
    margin-right: auto;
  }
  .test-result.ok {
    color: var(--success);
  }
  .test-result.fail {
    color: var(--danger);
  }

  .save-bar {
    display: flex;
    gap: 12px;
    align-items: center;
    justify-content: flex-end;
    max-width: 760px;
    margin-top: 8px;
  }
  .save-result {
    font-size: 13px;
    color: var(--success);
  }

  .stats-readout {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
    gap: 8px;
    margin-top: 16px;
    padding: 12px;
    background: var(--bg-2);
    border-radius: 8px;
    font-size: 13px;
  }
  .stats-readout span {
    color: var(--muted);
    margin-right: 6px;
  }

  .btn-danger {
    color: var(--danger);
    border-color: var(--danger);
  }

  .loading-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 24px;
    font-size: 14px;
    color: var(--muted);
    max-width: 760px;
  }

  .key-create {
    justify-content: flex-start;
    margin-top: 0;
    margin-bottom: 4px;
  }
  .key-name-input {
    flex: 1;
    max-width: 360px;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
    box-sizing: border-box;
  }
  .key-empty {
    font-size: 13px;
    color: var(--muted);
    margin: 8px 0 0;
  }
  .key-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 12px;
  }
  .key-row {
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-areas: 'main actions' 'value actions' 'meta actions';
    gap: 2px 12px;
    align-items: center;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
  }
  .key-row.revoked {
    opacity: 0.55;
  }
  .key-main {
    grid-area: main;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .key-name {
    font-weight: 600;
    font-size: 13px;
  }
  .key-tag.revoked {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--danger);
    border: 1px solid var(--danger);
    border-radius: 4px;
    padding: 1px 5px;
  }
  .key-value {
    grid-area: value;
    font-family: monospace;
    font-size: 12px;
    color: var(--ink);
  }
  .key-value.muted {
    color: var(--muted);
  }
  .key-meta {
    grid-area: meta;
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
    font-size: 11px;
    color: var(--muted);
  }
  .key-actions {
    grid-area: actions;
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .link-btn.danger {
    color: var(--danger);
  }
</style>
