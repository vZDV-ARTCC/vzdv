// Event times are stored and entered in Zulu. Show them in the viewer's local
// time with Zulu alongside, and preview Zulu form input in Denver time so
// daylight saving time mistakes stand out.
(() => {
  const localFormat = new Intl.DateTimeFormat("en-US", {
    year: "numeric",
    month: "long",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    timeZoneName: "short",
  });
  const denverFormat = new Intl.DateTimeFormat("en-US", {
    timeZone: "America/Denver",
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    timeZoneName: "short",
  });
  const denverOffsetFormat = new Intl.DateTimeFormat("en-US", {
    timeZone: "America/Denver",
    timeZoneName: "shortOffset",
  });

  const zulu = (date) => {
    const hour = date.getUTCHours().toString().padStart(2, "0");
    const minute = date.getUTCMinutes().toString().padStart(2, "0");
    return `${hour}${minute}z`;
  };

  const zoneName = (format, date) =>
    format.formatToParts(date).find((part) => part.type === "timeZoneName")
      .value;

  // e.g. { name: "MST", offset: "UTC-7" }
  const denverZone = (date) => ({
    name: zoneName(denverFormat, date),
    offset: zoneName(denverOffsetFormat, date).replace("GMT", "UTC"),
  });

  // Single times on the event page, which also prefill the edit form.
  document.querySelectorAll(".event-time").forEach((element) => {
    const date = new Date(element.innerText);
    element.innerText = `${localFormat.format(date)} (${zulu(date)})`;
    element.classList.remove("d-none");
    const target = document.getElementById(element.getAttribute("updateTarget"));
    if (target) {
      target.value = date.toISOString().slice(0, 16);
    }
  });

  // Start-end pairs in the upcoming events list.
  document.querySelectorAll(".event-time-range").forEach((element) => {
    const start = new Date(element.dataset.start);
    const end = new Date(element.dataset.end);
    element.innerText = `${localFormat.formatRange(start, end)} (${zulu(start)}–${zulu(end)})`;
  });

  // Zulu `datetime-local` inputs get a Denver time preview underneath, plus a
  // warning when Denver's UTC offset on that date differs from today's.
  const zuluInputs = document.querySelectorAll("input[data-zulu-input]");
  new Set([...zuluInputs].map((input) => input.form)).forEach((form) => {
    const inputs = [...form.querySelectorAll("input[data-zulu-input]")];
    const previews = inputs.map((input) => {
      const preview = document.createElement("div");
      preview.className = "form-text";
      input.after(preview);
      return preview;
    });
    const warning = document.createElement("div");
    warning.className = "col-12 form-text text-warning";
    inputs[0].closest(".row").append(warning);

    const update = () => {
      const today = denverZone(new Date());
      let changed = null;
      inputs.forEach((input, i) => {
        const date = new Date(`${input.value}Z`);
        if (Number.isNaN(date.getTime())) {
          previews[i].innerText = "";
          return;
        }
        previews[i].innerText = `Denver: ${denverFormat.format(date)}`;
        const zone = denverZone(date);
        if (zone.offset !== today.offset) {
          changed = zone;
        }
      });
      warning.hidden = !changed;
      warning.innerText = changed
        ? `Denver is on ${changed.name} (${changed.offset}) on this date, not ${today.name} (${today.offset}) like today. Double-check the Zulu times.`
        : "";
    };
    inputs.forEach((input) => input.addEventListener("input", update));
    update();
  });
})();
