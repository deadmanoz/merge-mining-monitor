const BLOCK_EXPLORERS = {
  bitcoin: {
    name: "mempool.space",
    blockUrl: ({ hash }) => (hash ? `https://mempool.space/block/${hash}` : null),
  },
  namecoin: {
    name: "Namebrow.se",
    blockUrl: ({ hash }) => (hash ? `https://www.namebrow.se/block/${hash}` : null),
  },
  rsk: {
    name: "Rootstock Explorer",
    blockUrl: ({ hash }) => {
      if (!hash) return null;
      const prefixed = hash.startsWith("0x") ? hash : `0x${hash}`;
      return `https://explorer.rootstock.io/blocks/${prefixed}`;
    },
  },
  syscoin: {
    name: "Syscoin Blockbook",
    blockUrl: ({ hash }) => {
      if (!hash) return null;
      return `https://explorer-blockbook.syscoin.org/block/${hash}`;
    },
  },
};

export function blockExplorer(chain, block = {}) {
  const explorer = BLOCK_EXPLORERS[normaliseChain(chain)];
  if (!explorer) return null;

  const url = explorer.blockUrl({
    hash: normaliseHash(block.hash),
    height: block.height,
  });
  if (!url) return null;

  return {
    name: explorer.name,
    url,
  };
}

function normaliseChain(chain) {
  return String(chain || "")
    .trim()
    .toLowerCase();
}

function normaliseHash(hash) {
  if (hash === null || hash === undefined) return null;
  const value = String(hash).trim();
  return value.length ? value : null;
}
