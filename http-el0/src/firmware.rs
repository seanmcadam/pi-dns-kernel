//! Firmware page. The page posts raw 16 KiB chunks. A hex form post is
//! still accepted for the small tests, and that body stays under 4096.
//! The kernel writes a temp file, reads it back, then renames.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use http_server::{HttpMethod, HttpRequest, HttpResponse};

const FW: u64 = 67;

struct View {
    staged: u32,
    bytes: u32,
    backup: u32,
    tryboot: u32,
    hw: u32,
    temp: u32,
    sha: String,
    result: String,
    staged_name: String,
    live_name: String,
    phase: String,
    progress: u32,
    total: u32,
    booted: String,
    boot_sha: String,
    bak: u32,
}

pub(super) fn handle(request: &HttpRequest) -> HttpResponse {
    if request.path == "/firmware" {
        return if request.method == HttpMethod::GET {
            page()
        } else {
            method_not()
        };
    }
    if request.path == "/firmware/status" {
        return if request.method == HttpMethod::GET {
            json_status()
        } else {
            method_not()
        };
    }
    if request.path == "/firmware/chunk" {
        return chunk(request);
    }
    if request.path == "/firmware/config" {
        return config(request);
    }
    if request.path == "/firmware/reboot" {
        return reboot(request);
    }
    if request.path == "/firmware/promote" {
        return promote(request);
    }
    if request.path == "/firmware/rollback" {
        return rollback(request);
    }
    if let Some((kind, offset)) = parse_read(&request.path) {
        return if request.method == HttpMethod::GET {
            read_file(kind, offset)
        } else {
            method_not()
        };
    }
    HttpResponse::not_found()
}

pub(super) fn fields() -> String {
    let version = version::VERSION;
    match view() {
        Some(view) => format!(
            "\"version\":\"{version}\",\"fw_staged\":{},\"fw_bytes\":{},\"fw_backup\":{},\"fw_tryboot\":{},\"fw_tryboot_hw\":{},\"fw_temp\":{},\"fw_sha256\":\"{}\",\"fw_result\":\"{}\",\"fw_phase\":\"{}\",\"fw_progress\":{},\"fw_total\":{},\"fw_booted\":\"{}\",\"fw_boot_sha256\":\"{}\",\"fw_bak\":{}",
            view.staged,
            view.bytes,
            view.backup,
            view.tryboot,
            view.hw,
            view.temp,
            view.sha,
            json_text(&view.result),
            json_text(&view.phase),
            view.progress,
            view.total,
            json_text(&view.booted),
            view.boot_sha,
            view.bak
        ),
        None => format!(
            "\"version\":\"{version}\",\"fw_staged\":0,\"fw_bytes\":0,\"fw_backup\":0,\"fw_tryboot\":0,\"fw_tryboot_hw\":0,\"fw_temp\":0,\"fw_sha256\":\"\",\"fw_result\":\"unavailable\",\"fw_phase\":\"idle\",\"fw_progress\":0,\"fw_total\":0,\"fw_booted\":\"unknown\",\"fw_boot_sha256\":\"\",\"fw_bak\":0"
        ),
    }
}

fn page() -> HttpResponse {
    let view = view();
    let (hw, name, live, bytes, backup, tryboot, temp, sha, result, phase, booted) = match view {
        Some(view) => (
            if view.hw == 1 { "yes" } else { "no" },
            view.staged_name,
            view.live_name,
            view.bytes,
            view.backup,
            view.tryboot,
            view.temp,
            view.sha,
            json_text(&view.result),
            json_text(&view.phase),
            json_text(&view.booted),
        ),
        None => (
            "no",
            String::from("KERNEL8.NEW"),
            String::from("KERNEL8.IMG"),
            0,
            0,
            0,
            0,
            String::new(),
            String::from("unavailable"),
            String::from("idle"),
            String::from("unknown"),
        ),
    };
    let version = version::VERSION;
    let mut html = format!(
        "<!DOCTYPE html><html><body>\
<h1>Firmware update</h1>\
<p>version {version}</p>\
<p>booted {booted}</p>\
<p>Uploads go to a temp name, are read back, and then replace only the staged file. \
<code>{live}</code> and <code>config.txt</code> are not written. \
The test file is <code>FWTEST.BIN</code>. The staged kernel is <code>{name}</code>.</p>\
<p>staged bytes {bytes}, backup present {backup}, tryboot.txt {tryboot}, temp {temp}</p>\
<p>tryboot hardware: {hw}</p>\
<p>sha256 {sha}</p>\
<p>result {result}</p>\
<p id=\"prog\">progress idle</p>\
<p><a href=\"/firmware/status\">Firmware status JSON</a></p>"
    );
    html.push_str(PAGE);
    html.push_str(&format!(
        "<script>if (\"{phase}\" === \"verifying\" || \"{phase}\" === \"staging\" || \"{phase}\" === \"receiving\" || \"{phase}\" === \"copying\") {{ pollVerify(); }}</script>"
    ));
    html.push_str("</body></html>");
    super::html_response(&html)
}

const PAGE: &str = r#"
<h2>Test file</h2>
<p>FWTEST.BIN is not a boot file. Choose a file, upload it, then read it back.</p>
<input id="testfile" type="file">
<button type="button" onclick="upload('test','testfile')">Upload test file</button>
<button type="button" onclick="readback()">Read test file back</button>
<h2>Kernel</h2>
<p>The new kernel is staged beside the live one. It is not copied over the live image.</p>
<input id="kernelfile" type="file">
<button type="button" onclick="upload('kernel','kernelfile')">Stage kernel</button>
<h2>Config</h2>
<p>Boot text is checked as tryboot.txt. JSON uses the existing config rules. An invalid document is not written.</p>
<p>Boot text</p>
<textarea id="boot" rows="6" cols="60"></textarea>
<p>
<button type="button" onclick="sendConfig('boot','0','boot')">Validate boot text</button>
<button type="button" onclick="sendConfig('boot','1','boot')">Write tryboot.txt</button>
</p>
<p>JSON config</p>
<textarea id="json" rows="6" cols="60"></textarea>
<p>
<button type="button" onclick="sendConfig('json','0','json')">Validate JSON</button>
<button type="button" onclick="sendConfig('json','1','json')">Commit JSON</button>
</p>
<form method="POST" action="/firmware/config">
<input type="hidden" name="preset" value="1">
<input type="hidden" name="commit" value="1">
<button type="submit">Write the recommended tryboot.txt</button>
</form>
<h2>Reboot</h2>
<form method="POST" action="/firmware/reboot" onsubmit="return confirm('Reboot this machine?');">
<label><input type="checkbox" name="confirm" value="reboot" required> Confirm reboot</label>
<input type="hidden" name="tryboot" value="0">
<button type="submit">Reboot</button>
</form>
<form method="POST" action="/firmware/reboot" onsubmit="return confirm('Reboot this machine into tryboot.txt?');">
<label><input type="checkbox" name="confirm" value="reboot" required> Confirm tryboot reboot</label>
<input type="hidden" name="tryboot" value="1">
<button type="submit">Tryboot reboot</button>
</form>
<h2>Promote and rollback</h2>
<p>Promote copies the live kernel to KERNEL8.BAK, then installs the staged kernel as the live file. It runs only after a tryboot boot, unless force is checked. Rollback restores KERNEL8.BAK.</p>
<form method="POST" action="/firmware/promote" onsubmit="return confirm('Promote the staged kernel?');">
<label><input type="checkbox" name="confirm" value="promote" required> Confirm promote</label>
<label><input type="checkbox" name="force" value="1"> Force</label>
<button type="submit">Promote</button>
</form>
<form method="POST" action="/firmware/rollback" onsubmit="return confirm('Roll back to KERNEL8.BAK?');">
<label><input type="checkbox" name="confirm" value="rollback" required> Confirm rollback</label>
<button type="submit">Rollback</button>
</form>
<pre id="out"></pre>
<script>
function setOut(text) {
  document.getElementById("out").textContent = text;
  var prog = document.getElementById("prog");
  if (prog) prog.textContent = text;
}
function upload(kind, id) {
  var file = document.getElementById(id).files[0];
  if (!file) { setOut("choose a file"); return; }
  var reader = new FileReader();
  reader.onload = function() {
    var buf = new Uint8Array(reader.result);
    sendChunks(kind, buf, 0, 0);
  };
  reader.readAsArrayBuffer(file);
}
function sendChunks(kind, buf, off, tries) {
  var step = 16384;
  var piece = buf.subarray(off, Math.min(off + step, buf.length));
  var final = (off + piece.length === buf.length) ? "1" : "0";
  setOut("upload " + off + "/" + buf.length);
  fetch("/firmware/chunk", {method:"POST", headers:{"Content-Type":"application/octet-stream", "X-Fw-Kind":kind, "X-Fw-Offset":String(off), "X-Fw-Total":String(buf.length), "X-Fw-Final":final}, body: piece})
    .then(function(res) { return res.text().then(function(text) { return {ok: res.ok, text: text}; }); })
    .then(function(ans) {
      if (!ans.ok) {
        if (tries < 3) { setOut("retry " + off + "/" + buf.length); sendChunks(kind, buf, off, tries + 1); return; }
        setOut(ans.text);
        return;
      }
      if (final === "1") {
        var doc = {};
        try { doc = JSON.parse(ans.text); } catch (e) { doc = {}; }
        if (doc.status === "verifying") { pollVerify(); return; }
        setOut(ans.text);
        return;
      }
      sendChunks(kind, buf, off + piece.length, 0);
    })
    .catch(function() {
      if (tries < 3) sendChunks(kind, buf, off, tries + 1);
      else setOut("upload failed");
    });
}
function pollVerify() {
  fetch("/firmware/status").then(function(res) { return res.json(); }).then(function(doc) {
    var phase = doc.fw_phase || "";
    var prog = doc.fw_progress || 0;
    var total = doc.fw_total || 0;
    setOut(phase + " " + prog + "/" + total);
    if (phase === "verifying" || phase === "staging" || phase === "receiving" || phase === "copying") {
      setTimeout(pollVerify, 50);
    }
  }).catch(function() { setTimeout(pollVerify, 200); });
}
function readback() {
  readFrom(0, []);
}
function readFrom(off, parts) {
  fetch("/firmware/read/test/" + off).then(function(res) {
    var total = Number(res.headers.get("X-Fw-Total") || "0");
    var sha = res.headers.get("X-Fw-Sha256") || "";
    return res.arrayBuffer().then(function(buf) {
      if (!res.ok) { setOut("read failed"); return; }
      var bytes = new Uint8Array(buf);
      if (bytes.length === 0 || off >= total) {
        var n = 0;
        for (var i = 0; i < parts.length; i++) n += parts[i].length;
        setOut("read " + n + " bytes sha256 " + sha);
        return;
      }
      parts.push(bytes);
      readFrom(off + bytes.length, parts);
    });
  });
}
function sendConfig(kind, commit, id) {
  var text = document.getElementById(id).value;
  fetch("/firmware/config", {method:"POST", headers:{"X-Fw-Kind":kind, "X-Fw-Commit":commit, "Content-Type":"text/plain"}, body: text})
    .then(function(res) { return res.text(); })
    .then(function(text) { setOut(text); });
}
</script>
"#;

fn json_status() -> HttpResponse {
    let body = format!("{{{}}}\n", fields());
    let mut response = HttpResponse::ok(&body);
    response.add_header("Content-Type", "application/json");
    response
}

fn chunk(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return method_not();
    }
    if request
        .get_header("Content-Type")
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
    {
        return chunk_raw(request);
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    let kind = match form_value(body, "kind") {
        Some("test") => 1,
        Some("kernel") => 2,
        _ => return super::bad_request("kind must be test or kernel"),
    };
    let offset: u64 = match form_value(body, "offset").and_then(|text| text.parse().ok()) {
        Some(value) => value,
        None => return super::bad_request("offset required"),
    };
    let total: u64 = match form_value(body, "total").and_then(|text| text.parse().ok()) {
        Some(value) => value,
        None => return super::bad_request("total required"),
    };
    let data_hex = form_value(body, "data").unwrap_or("");
    let mut data = [0u8; 1792];
    let Some(n) = decode_hex(data_hex, &mut data) else {
        return super::bad_request("data must be hex");
    };
    if n == 0 {
        return super::bad_request("empty chunk");
    }
    store_chunk(
        kind,
        offset,
        total,
        &data[..n],
        form_value(body, "final") == Some("1"),
        form_value(body, "sha256"),
    )
}

fn chunk_raw(request: &HttpRequest) -> HttpResponse {
    let kind = match request.get_header("X-Fw-Kind") {
        Some("test") => 1,
        Some("kernel") => 2,
        _ => return super::bad_request("kind must be test or kernel"),
    };
    let offset: u64 = match request
        .get_header("X-Fw-Offset")
        .and_then(|text| text.parse().ok())
    {
        Some(value) => value,
        None => return super::bad_request("offset required"),
    };
    let total: u64 = match request
        .get_header("X-Fw-Total")
        .and_then(|text| text.parse().ok())
    {
        Some(value) => value,
        None => return super::bad_request("total required"),
    };
    if request.body.is_empty() {
        return super::bad_request("empty chunk");
    }
    store_chunk(
        kind,
        offset,
        total,
        &request.body,
        request.get_header("X-Fw-Final") == Some("1"),
        request.get_header("X-Fw-Sha256"),
    )
}

fn store_chunk(
    kind: u64,
    offset: u64,
    total: u64,
    data: &[u8],
    final_chunk: bool,
    sha_hex: Option<&str>,
) -> HttpResponse {
    let rc = super::sys(
        FW,
        1,
        kind,
        offset,
        total,
        data.as_ptr() as u64,
        data.len() as u64,
    );
    if rc < 0 {
        return fw_error(rc);
    }
    if !final_chunk {
        let mut response = HttpResponse::ok("{\"status\":\"ok\"}\n");
        response.add_header("Content-Type", "application/json");
        return response;
    }
    let rc = if let Some(hexed) = sha_hex {
        let mut sha = [0u8; 32];
        if decode_hex(hexed, &mut sha) != Some(32) {
            return super::bad_request("sha256 must be 32 bytes of hex");
        }
        super::sys(FW, 2, kind, sha.as_ptr() as u64, 0, 0, 0)
    } else {
        super::sys(FW, 9, kind, 0, 0, 0, 0)
    };
    if rc < 0 {
        return fw_error(rc);
    }
    if rc == 1 {
        let view = view();
        let (progress, total) = match view {
            Some(view) => (view.progress, view.total),
            None => (0, 0),
        };
        let mut response = HttpResponse::ok(&format!(
            "{{\"status\":\"verifying\",\"progress\":{progress},\"total\":{total}}}\n"
        ));
        response.add_header("Content-Type", "application/json");
        return response;
    }
    let mut response = HttpResponse::ok("{\"status\":\"staged\"}\n");
    response.add_header("Content-Type", "application/json");
    response
}

fn config(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return method_not();
    }
    if let Some(kind) = request.get_header("X-Fw-Kind") {
        let commit = request.get_header("X-Fw-Commit").unwrap_or("0");
        let text = core::str::from_utf8(&request.body).unwrap_or("");
        return apply_config(kind, commit, text);
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "preset") == Some("1") {
        let commit: u64 = if form_value(body, "commit") == Some("1") {
            1
        } else {
            0
        };
        return preset(commit);
    }
    let kind = form_value(body, "kind").unwrap_or("");
    let commit = form_value(body, "commit").unwrap_or("0");
    let text = percent_decode(form_value(body, "text").unwrap_or(""));
    apply_config(kind, commit, &text)
}

fn apply_config(kind: &str, commit: &str, text: &str) -> HttpResponse {
    if text.len() > 4096 {
        return super::bad_request("too large");
    }
    let kind = match kind {
        "boot" => 3,
        "json" => 4,
        "log" => 5,
        _ => return super::bad_request("kind must be boot, json, or log"),
    };
    let commit: u64 = if commit == "1" { 1 } else { 0 };
    let mut err = [0u8; 160];
    let rc = super::sys(
        FW,
        5,
        kind,
        text.as_ptr() as u64,
        text.len() as u64,
        commit,
        err.as_mut_ptr() as u64,
    );
    config_result(rc, &err, commit == 1)
}

fn preset(commit: u64) -> HttpResponse {
    let mut err = [0u8; 160];
    let rc = super::sys(FW, 8, commit, err.as_mut_ptr() as u64, 0, 0, 0);
    config_result(rc, &err, commit == 1)
}

fn config_result(rc: i64, err: &[u8], committed: bool) -> HttpResponse {
    if rc < 0 {
        let message = cstr(err);
        if message.is_empty() {
            return fw_error(rc);
        }
        return super::bad_request(message);
    }
    let word = if committed { "stored" } else { "valid" };
    let mut response = HttpResponse::ok(&format!("{{\"status\":\"{word}\"}}\n"));
    response.add_header("Content-Type", "application/json");
    response
}

fn reboot(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return method_not();
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("reboot") {
        return super::bad_request("Confirmation required");
    }
    let tryboot: u64 = if form_value(body, "tryboot") == Some("1") {
        1
    } else {
        0
    };
    let rc = super::sys(FW, 6, tryboot, 0, 0, 0, 0);
    if rc < 0 {
        return match (tryboot, rc) {
            (1, -5) => super::bad_request("tryboot is not supported on this board"),
            (_, -4) => super::bad_request("staged kernel or tryboot.txt is not ready"),
            (_, -3) => super::bad_request("Power request was rejected"),
            _ => fw_error(rc),
        };
    }
    if rc == 1 {
        return super::html_ok("A power change is already requested.");
    }
    let word = if tryboot == 1 {
        "Tryboot reboot requested."
    } else {
        "Reboot requested."
    };
    super::html_ok(word)
}

fn read_file(kind: u64, offset: u64) -> HttpResponse {
    let mut sha = [0u8; 32];
    let total = super::sys(FW, 7, kind, sha.as_mut_ptr() as u64, 0, 0, 0);
    if total < 0 {
        return HttpResponse::not_found();
    }
    let mut buf = [0u8; 512];
    let n = super::sys(
        FW,
        4,
        kind,
        offset,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
    );
    if n < 0 {
        return HttpResponse::not_found();
    }
    let mut response = HttpResponse::new(200, "OK");
    response.set_body(&buf[..n as usize]);
    response.add_header("Content-Type", "application/octet-stream");
    response.add_header("X-Fw-Total", &total.to_string());
    response.add_header("X-Fw-Offset", &offset.to_string());
    response.add_header("X-Fw-Sha256", &hex_encode(&sha));
    response
}

fn promote(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return method_not();
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("promote") {
        return super::bad_request("Confirmation required");
    }
    let force: u64 = if form_value(body, "force") == Some("1") {
        1
    } else {
        0
    };
    let rc = super::sys(FW, 10, force, 0, 0, 0, 0);
    if rc < 0 {
        return match rc {
            -14 => super::bad_request("promote is not supported on this board"),
            -12 => super::bad_request("promote needs a tryboot boot or force"),
            -4 => super::bad_request("staged kernel is not ready"),
            -3 => super::bad_request("a firmware copy is already running"),
            -5 => super::bad_request("no card"),
            _ => fw_error(rc),
        };
    }
    if rc == 1 {
        return super::html_ok("Promote is already running.");
    }
    super::html_ok("Promote started.")
}

fn rollback(request: &HttpRequest) -> HttpResponse {
    if request.method != HttpMethod::POST {
        return method_not();
    }
    let body = core::str::from_utf8(&request.body).unwrap_or("");
    if form_value(body, "confirm") != Some("rollback") {
        return super::bad_request("Confirmation required");
    }
    let rc = super::sys(FW, 11, 0, 0, 0, 0, 0);
    if rc < 0 {
        return match rc {
            -14 => super::bad_request("rollback is not supported on this board"),
            -13 => super::bad_request("rollback needs a backup"),
            -3 => super::bad_request("a firmware copy is already running"),
            -5 => super::bad_request("no card"),
            _ => fw_error(rc),
        };
    }
    if rc == 1 {
        return super::html_ok("Rollback is already running.");
    }
    super::html_ok("Rollback started.")
}

fn view() -> Option<View> {
    let mut raw = [0u8; 224];
    let n = super::sys(FW, 3, raw.as_mut_ptr() as u64, raw.len() as u64, 0, 0, 0);
    if n < 224 {
        return None;
    }
    let sha = if raw[104] == 1 {
        hex_encode(&raw[24..56])
    } else {
        String::new()
    };
    let boot_sha = if raw[176] == 1 {
        hex_encode(&raw[177..209])
    } else {
        String::new()
    };
    Some(View {
        staged: u32_at(&raw, 0),
        bytes: u32_at(&raw, 4),
        backup: u32_at(&raw, 8),
        tryboot: u32_at(&raw, 12),
        hw: u32_at(&raw, 16),
        temp: u32_at(&raw, 20),
        sha,
        result: cstr(&raw[56..104]).into(),
        staged_name: cstr(&raw[105..121]).into(),
        live_name: cstr(&raw[121..137]).into(),
        phase: cstr(&raw[137..152]).into(),
        progress: u32_at(&raw, 152),
        total: u32_at(&raw, 156),
        booted: cstr(&raw[160..176]).into(),
        boot_sha,
        bak: u32_at(&raw, 212),
    })
}

fn fw_error(rc: i64) -> HttpResponse {
    let message = match rc {
        -5 => "no card",
        -6 => "checksum mismatch",
        -7 => "image rejected",
        -10 => "bad offset",
        -11 => "too large",
        _ => "rejected",
    };
    super::bad_request(message)
}

fn method_not() -> HttpResponse {
    HttpResponse::new(405, "Method Not Allowed")
}

fn u32_at(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

fn cstr(raw: &[u8]) -> &str {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    core::str::from_utf8(&raw[..end]).unwrap_or("")
}

fn json_text(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == ' ' || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        }
    }
    if out.is_empty() {
        out.push_str("idle");
    }
    out
}

fn hex_encode(src: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new();
    for byte in src {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn decode_hex(text: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 || bytes.len() / 2 > out.len() {
        return None;
    }
    let n = bytes.len() / 2;
    let mut i = 0;
    while i < n {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(n)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    for pair in body.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name.trim() == key {
                return Some(value.trim());
            }
        }
    }
    None
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn parse_read(path: &str) -> Option<(u64, u64)> {
    let rest = path.strip_prefix("/firmware/read/")?;
    let (kind, offset) = rest.split_once('/')?;
    let kind = match kind {
        "test" => 1,
        "kernel" => 2,
        "tryboot" => 3,
        "live" => 6,
        "bak" => 7,
        _ => return None,
    };
    let offset = offset.parse().ok()?;
    Some((kind, offset))
}
