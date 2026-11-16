(function () {
  let lastCount = -1;

  function getUnreadCount() {
    const match = document.title.match(/^\((\d+)\)/);
    return match ? parseInt(match[1], 10) : 0;
  }

  function renderBadgeRGBA(count) {
    const size = 32;
    const canvas = document.createElement('canvas');
    canvas.width = size;
    canvas.height = size;
    const ctx = canvas.getContext('2d');

    // Red circle (Google style)
    ctx.fillStyle = '#D93025';
    ctx.beginPath();
    ctx.arc(size / 2, size / 2, size / 2, 0, Math.PI * 2);
    ctx.fill();

    // Unread count label
    const label = count > 9 ? '9+' : String(count);
    ctx.fillStyle = '#FFFFFF';
    ctx.font = label.length > 1 ? 'bold 17px Segoe UI, sans-serif' : 'bold 20px Segoe UI, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(label, size / 2, size / 2 + 1);

    return new Uint8Array(ctx.getImageData(0, 0, size, size).data.buffer);
  }

  async function syncBadge() {
    if (!window.__TAURI__?.window || !window.__TAURI__?.image) return;
    const count = getUnreadCount();
    if (count === lastCount) return;
    lastCount = count;

    try {
      const win = window.__TAURI__.window.getCurrentWindow();
      if (count > 0) {
        const rgba = renderBadgeRGBA(count);
        const icon = await window.__TAURI__.image.Image.new(rgba, 32, 32);
        await win.setOverlayIcon(icon);
        await icon.close();
      } else {
        await win.setOverlayIcon();
      }
    } catch (_) {}
  }

  window.addEventListener('DOMContentLoaded', () => {
    const titleEl = document.querySelector('title');
    if (titleEl) {
      new MutationObserver(syncBadge).observe(titleEl, { childList: true, characterData: true, subtree: true });
    }
    setInterval(syncBadge, 2000);
    syncBadge();
  });
})();
