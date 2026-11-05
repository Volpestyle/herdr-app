// Playwright half of codec_smoke.py: measure real playback through the guarded bridge.
// Usage: node codec_playwright.mjs <playwright-core dir> <endpoint> <options json>
import { createRequire } from 'node:module';

const [core, endpoint, optionsJson] = process.argv.slice(2);
const options = JSON.parse(optionsJson);
const { chromium } = createRequire(import.meta.url)(core);
const MP4 = 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"';

// Plays the page's first video for a few seconds and reports what decoded.
const measure = (page) => page.evaluate(async (type) => {
  const video = document.querySelector('video');
  const support = { canPlayType: document.createElement('video').canPlayType(type), mediaSource: globalThis.MediaSource?.isTypeSupported(type) ?? null };
  if (!video) return { ...support, error: 'no video element', advanced: 0, frames: 0 };
  video.muted = true;
  let playError = null;
  try { await video.play(); } catch (error) { playError = String(error); }
  await new Promise((done) => setTimeout(done, 1000));
  const start = video.currentTime;
  await new Promise((done) => setTimeout(done, 4000));
  const quality = video.getVideoPlaybackQuality();
  return {
    ...support, src: video.currentSrc, playError, mediaError: video.error?.message ?? video.error?.code ?? null,
    start, end: video.currentTime, advanced: video.currentTime - start, paused: video.paused, readyState: video.readyState,
    videoWidth: video.videoWidth, videoHeight: video.videoHeight,
    frames: quality.totalVideoFrames, dropped: quality.droppedVideoFrames,
    audioBytes: video.webkitAudioDecodedByteCount, videoBytes: video.webkitVideoDecodedByteCount,
  };
}, MP4);

const report = { public: [] };
const browser = await chromium.connectOverCDP(endpoint);
try {
  report.version = browser.version();
  const context = browser.contexts()[0];
  const page = context.pages().find((p) => p.url().startsWith(options.fixture));
  if (!page) throw new Error('fixture page not visible');
  report.local = await measure(page);
  await page.screenshot({ path: `${options.shots}/local.png` });
  for (const [index, url] of options.public.entries()) {
    // A wrapper page, because a bare media URL the build cannot play becomes a download.
    await page.setContent(`<!doctype html><body style="margin:0;background:#111"><video src="${url}" muted loop style="width:640px"></video>`);
    report.public.push({ url, ...(await measure(page)) });
    await page.screenshot({ path: `${options.shots}/public-${index}.png` });
  }
  if (options.youtube) {
    await page.goto(options.youtube, { waitUntil: 'domcontentloaded' });
    await page.locator('video').first().waitFor({ state: 'attached', timeout: 30000 });
    await page.waitForTimeout(3000);
    report.youtube = { url: options.youtube, ...(await measure(page)) };
    report.youtube.stats = await page.evaluate(() => {
      const player = document.querySelector('#movie_player');
      const stats = player?.getStatsForNerds?.();
      return stats ? { codecs: stats.codecs, resolution: stats.resolution } : null;
    });
    await page.screenshot({ path: `${options.shots}/youtube.png` });
  }
} finally {
  await browser.close();
}
console.log(JSON.stringify(report));
