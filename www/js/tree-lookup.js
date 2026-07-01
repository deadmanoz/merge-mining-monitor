function utcDateTimeToInput(value) {
  const utc = inputDateTimeToUtc(value);
  return utc ? utc.slice(0, -1) : "";
}

function inputDateTimeToUtc(value) {
  const text = String(value || "").trim();
  if (text === "") return "";
  const match = text.match(
    /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?(?:Z|z)?$/,
  );
  if (!match) return "";
  const [, yearText, monthText, dayText, hourText, minuteText, secondText = "00"] = match;
  const year = Number(yearText);
  const month = Number(monthText);
  const day = Number(dayText);
  const hour = Number(hourText);
  const minute = Number(minuteText);
  const second = Number(secondText);
  const date = new Date(Date.UTC(year, month - 1, day, hour, minute, second));
  if (
    date.getUTCFullYear() !== year
    || date.getUTCMonth() !== month - 1
    || date.getUTCDate() !== day
    || date.getUTCHours() !== hour
    || date.getUTCMinutes() !== minute
    || date.getUTCSeconds() !== second
  ) {
    return "";
  }
  return `${yearText}-${monthText}-${dayText}T${hourText}:${minuteText}:${secondText}Z`;
}

export { utcDateTimeToInput, inputDateTimeToUtc };
