// Native <dialog> open/close and ARIA modal tab primitives.

export function showDialog(dialog) {
  if (typeof dialog.showModal === "function") {
    dialog.showModal();
  } else {
    dialog.setAttribute("open", "");
  }
}

export function closeDialog(dialog) {
  if (typeof dialog.close === "function") {
    dialog.close();
  } else {
    dialog.removeAttribute("open");
  }
}

function modalTabPanel(root, tab) {
  const id = tab.getAttribute("aria-controls");
  return id ? root.querySelector(`#${CSS.escape(id)}`) : null;
}

export function activateModalTab(root, tabId) {
  root.querySelectorAll(".modal-tab").forEach((tab) => {
    const isActive = tab.dataset.tab === tabId;
    tab.setAttribute("aria-selected", String(isActive));
    if (isActive) tab.removeAttribute("tabindex");
    else tab.setAttribute("tabindex", "-1");
    const panel = modalTabPanel(root, tab);
    if (panel) panel.hidden = !isActive;
  });
}

export function wireModalTabs(dialog) {
  if (!dialog) return;
  dialog.addEventListener("click", (event) => {
    const tab = event.target.closest(".modal-tab");
    if (tab && dialog.contains(tab)) activateModalTab(dialog, tab.dataset.tab);
  });
  dialog.addEventListener("keydown", (event) => {
    const tab = event.target.closest(".modal-tab");
    if (!tab) return;
    const tabs = [...dialog.querySelectorAll(".modal-tab")];
    const index = tabs.indexOf(tab);
    let next = -1;
    if (event.key === "ArrowRight") next = (index + 1) % tabs.length;
    else if (event.key === "ArrowLeft") next = (index - 1 + tabs.length) % tabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = tabs.length - 1;
    if (next < 0) return;
    event.preventDefault();
    tabs[next].focus();
    activateModalTab(dialog, tabs[next].dataset.tab);
  });
}
