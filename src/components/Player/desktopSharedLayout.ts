export const getDesktopPlayerSharedLayoutIds = (songId: unknown) => {
  const songScope = String(songId ?? "empty");

  return {
    cover: `desktop-player-cover-${songScope}`,
    title: `desktop-player-title-${songScope}`,
    artists: `desktop-player-artists-${songScope}`,
  } as const;
};
