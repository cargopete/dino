const $ = (id) => document.getElementById(id);
const ask = (message) => chrome.runtime.sendMessage(message);

async function refresh() {
  const status = await ask({ what: "status" });
  $("state").textContent = status.connected ? `connected · ${status.tabs} tab${status.tabs === 1 ? "" : "s"} in use` : status.token ? "Dino is not listening" : "not set up";
  $("state").className = status.connected ? "state on" : "state";
  $("port").value = status.port;
  $("token").value = status.token;
  $("setup").open = !status.token;
}

$("share").onclick = async () => {
  const reply = await ask({ what: "share" });
  $("note").textContent = reply.ok ? "sent; Dino can read this tab until you close it or press Cancel on the bar" : reply.why;
  refresh();
};
$("save").onclick = async () => {
  await ask({ what: "save", port: $("port").value, token: $("token").value });
  setTimeout(refresh, 500);
};
refresh();
