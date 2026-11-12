import { Fragment as e, computed as t, createBlock as n, createCommentVNode as r, createElementBlock as i, createElementVNode as a, createTextVNode as o, createVNode as s, defineComponent as c, isRef as l, nextTick as u, normalizeClass as d, normalizeStyle as f, onBeforeUnmount as p, onMounted as m, openBlock as h, ref as g, renderList as _, resolveDynamicComponent as ee, toDisplayString as v, unref as y, vModelText as te, watch as b, withCtx as x, withDirectives as ne, withKeys as S, withModifiers as C } from "vue";
import { NeumorphismBadge as re, NeumorphismCanvas as ie, NeumorphismModal as w, NeumorphismPopover as ae, NeumorphismThemeToggle as T, NeumorphismToastProvider as E, useTheme as D, useTouchDevice as oe } from "@echolab-auto/ui-frame";
import { DocFlowCanvas as se, DocViewer as O, MarkdownEditor as ce, MarkdownRenderer as le, MarkdownRenderer as ue, writeFlowNodePosition as de } from "@echolab-auto/ui-frame/doc";
import "@echolab-auto/ui-frame/dist/style.css";
//#region src/composables/usePointerSession.ts
var fe = [];
function pe(e) {
	return e.cancel ?? e.end;
}
function k(e) {
	e.move && window.addEventListener("pointermove", e.move), window.addEventListener("pointerup", e.end), window.addEventListener("pointercancel", pe(e)), fe.some((t) => t.end === e.end) || fe.push(e);
}
function A(e) {
	e.move && window.removeEventListener("pointermove", e.move), window.removeEventListener("pointerup", e.end), window.removeEventListener("pointercancel", pe(e));
	let t = fe.findIndex((t) => t.end === e.end);
	t !== -1 && fe.splice(t, 1);
}
function me() {
	for (let e of [...fe]) A(e);
}
//#endregion
//#region ../prodoc-core/dist/search-AsFgefFm.js
function j(e) {
	let t = [], n = "", r = null;
	for (let i of e) r ? (n += i, i === r && (r = null)) : i === "\"" || i === "'" ? (r = i, n += i) : i === "," ? (t.push(n), n = "") : n += i;
	return t.push(n), t;
}
function he(e) {
	let t = e.trim();
	if (t === "") return "";
	if (t.startsWith("[") && t.endsWith("]")) return j(t.slice(1, -1)).map((e) => he(e)).filter((e) => e !== "");
	if (t === "true") return !0;
	if (t === "false") return !1;
	if (/^-?\d+$/.test(t)) return parseInt(t, 10);
	if (/^-?\d*\.\d+$/.test(t)) return parseFloat(t);
	let n = t.match(/^(["'])([\s\S]*)\1$/);
	return n ? n[2] : t;
}
function ge(e) {
	let t = e.indexOf("\n");
	if ((t === -1 ? e : e.slice(0, t)).trim() !== "---") return {
		params: {},
		body: e,
		hasFrame: !1
	};
	let n = (t === -1 ? "" : e.slice(t + 1)).split("\n"), r = -1;
	for (let e = 0; e < n.length; e++) if (n[e].trim() === "---") {
		r = e;
		break;
	}
	if (r === -1) return {
		params: {},
		body: e,
		hasFrame: !1
	};
	let i = {}, a = 0, o = n.slice(0, r);
	for (; a < o.length;) {
		let e = o[a];
		if (a += 1, e.trim() === "") continue;
		let t = e.match(/^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*([\s\S]*)$/);
		if (!t) continue;
		let n = t[1], r = t[2];
		if (r.trim() === "" && a < o.length && o[a].trimStart().startsWith("[") && (r = o[a], a += 1), r.trimStart().startsWith("[") && !/\]\s*$/.test(r)) {
			let e = [r];
			for (; a < o.length;) {
				let t = o[a];
				if (a += 1, e.push(t), /\]\s*$/.test(t)) break;
			}
			r = e.join("\n");
		}
		i[n] = he(r);
	}
	return {
		params: i,
		body: n.slice(r + 1).join("\n"),
		hasFrame: !0
	};
}
function _e(e) {
	return (Array.isArray(e) ? e : typeof e == "string" ? e.split(/,\s+/) : []).filter((e) => typeof e == "string").map((e) => e.trim()).filter(Boolean);
}
function ve(e) {
	return _e(ge(e).params.link);
}
function ye(e) {
	return e === "" || e !== e.trim() || /[|,"]/.test(e) ? e.includes("\"") ? `'${e}'` : `"${e}"` : e;
}
function be(e, t, n) {
	let r = e.includes("\r\n") ? "\r\n" : "\n";
	if (!ge(e).hasFrame) return n === null ? e : `---${r}${n}${r}---${r}${e}`;
	let i = e.split(/\r?\n/), a = i.findIndex((e, t) => t > 0 && e.trim() === "---");
	if (a === -1) return e;
	let o = RegExp(`^${t}\\s*:`), s = i.slice(1, a).findIndex((e) => o.test(e));
	if (s >= 0) {
		let e = s + 1, t = 1, r = e, c = i[e].replace(o, "");
		if (c.trim() === "" && e + 1 < a && i[e + 1].trimStart().startsWith("[") && (r = e + 1, c = i[r], t = 2), c.trimStart().startsWith("[") && !/\]\s*$/.test(c)) {
			let n = r + 1;
			for (; n < a && !/\]\s*$/.test(i[n]);) n++;
			n < a && (t = n - e + 1);
		}
		n === null ? i.splice(e, t) : i.splice(e, t, n);
	} else n !== null && i.splice(a, 0, n);
	return i.join(r);
}
function xe(e, t) {
	return be(e, "link", t.length > 0 ? `link: [${t.map(ye).join(", ")}]` : null);
}
function Se(e, t) {
	return be(e, "group", t === null ? null : `group: ${ye(t)}`);
}
function Ce(e, t) {
	let n = Object.entries(t).filter((e) => (e[0] === "x" || e[0] === "y") && typeof e[1] == "number" && Number.isFinite(e[1]));
	if (n.length === 0) return e;
	let r = e.includes("\r\n") ? "\r\n" : "\n";
	if (!ge(e).hasFrame) return `---${r}${n.map(([e, t]) => `${e}: ${t}`).join(r)}${r}---${r}${e}`;
	let i = e.split(/\r?\n/), a = i.findIndex((e, t) => t > 0 && e.trim() === "---");
	if (a === -1) return e;
	let o = a;
	for (let [e, t] of n) {
		let n = RegExp(`^${e}\\s*:`), r = i.slice(1, o).findIndex((e) => n.test(e));
		r >= 0 ? i[r + 1] = `${e}: ${t}` : (i.splice(o, 0, `${e}: ${t}`), o++);
	}
	return i.join(r);
}
function we(e, t, n) {
	let r = e.length, i = /* @__PURE__ */ new Map();
	e.forEach((e, t) => i.set(e, t));
	let a = Array.from({ length: r }, () => []);
	for (let e of t) {
		let t = i.get(e.from), n = i.get(e.to);
		t !== void 0 && n !== void 0 && t !== n && a[t].push(n);
	}
	let o = new Int32Array(r).fill(-1), s = new Int32Array(r).fill(-1), c = new Int32Array(r), l = new Uint8Array(r), u = [], d = 0, f = 0;
	for (let e = 0; e < r; e++) {
		if (s[e] !== -1) continue;
		let t = [{
			v: e,
			ei: 0
		}];
		for (s[e] = c[e] = d++, u.push(e), l[e] = 1; t.length > 0;) {
			let e = t[t.length - 1], n = e.v;
			if (e.ei < a[n].length) {
				let r = a[n][e.ei++];
				s[r] === -1 ? (s[r] = c[r] = d++, u.push(r), l[r] = 1, t.push({
					v: r,
					ei: 0
				})) : l[r] && (c[n] = Math.min(c[n], s[r]));
				continue;
			}
			if (t.pop(), t.length > 0) {
				let e = t[t.length - 1].v;
				c[e] = Math.min(c[e], c[n]);
			}
			if (c[n] === s[n]) {
				for (;;) {
					let e = u.pop();
					if (l[e] = 0, o[e] = f, e === n) break;
				}
				f++;
			}
		}
	}
	let p = Array.from({ length: f }, () => /* @__PURE__ */ new Set()), m = Array.from({ length: f }, () => /* @__PURE__ */ new Set());
	for (let e = 0; e < r; e++) for (let t of a[e]) o[e] !== o[t] && (p[o[e]].add(o[t]), m[o[t]].add(o[e]));
	let h = new Int32Array(f), g = Int32Array.from({ length: f }, (e, t) => m[t].size), _ = [];
	for (let e = 0; e < f; e++) g[e] === 0 && _.push(e);
	for (let e = 0; e < _.length; e++) {
		let t = _[e];
		for (let e of m[t]) h[t] = Math.max(h[t], h[e] + 1);
		for (let e of p[t]) --g[e] === 0 && _.push(e);
	}
	if (n) {
		let t = /* @__PURE__ */ new Map();
		for (let n = 0; n < r; n++) {
			let r = t.get(o[n]);
			r ? r.push(e[n]) : t.set(o[n], [e[n]]);
		}
		for (let e of t.values()) e.length > 1 && n.push(`检测到循环连线（${e.join("、")}）：环上文档排在同一层`);
	}
	let ee = /* @__PURE__ */ new Map();
	for (let t = 0; t < r; t++) ee.set(e[t], h[o[t]]);
	return ee;
}
function M(e, t, n) {
	let r = /* @__PURE__ */ new Map();
	t.forEach((e, t) => r.set(e, t));
	let i = [];
	for (let t of e) {
		let e = [];
		for (let i of n.get(t) ?? []) {
			let t = r.get(i);
			t !== void 0 && e.push(t);
		}
		e.sort((e, t) => e - t), i.push(...e);
	}
	return N(i);
}
function N(e) {
	if (e.length < 2) return 0;
	let t = Array(e.length), n = 0, r = (i, a) => {
		if (a - i <= 1) return;
		let o = i + a >> 1;
		r(i, o), r(o, a);
		let s = i, c = o, l = i;
		for (; s < o && c < a;) e[s] <= e[c] ? t[l++] = e[s++] : (n += o - s, t[l++] = e[c++]);
		for (; s < o;) t[l++] = e[s++];
		for (; c < a;) t[l++] = e[c++];
		for (let n = i; n < a; n++) e[n] = t[n];
	};
	return r(0, e.length), n;
}
function P(e, t) {
	let n = 0;
	for (let r = 0; r + 1 < e.length; r++) n += M(e[r], e[r + 1], t);
	return n;
}
function Te(e, t, n = {}, r) {
	let i = n.padding ?? 48, a = n.gapX ?? 64, o = n.gapY ?? 72, s = n.iterations ?? 4, c = n.avoidFixed ?? !0, l = n.wrapThreshold ?? Infinity;
	new Map(e.map((e) => [e.id, e]));
	let u = e.map((e) => e.id), d = we(u, t, r), f = u.filter((e) => !t.some((t) => t.to === e)), p = /* @__PURE__ */ new Map();
	for (let t of e) p.set(t.id, {
		id: t.id,
		dummy: !1,
		layer: d.get(t.id) ?? 0,
		w: t.w,
		h: t.h,
		order: u.indexOf(t.id),
		fixedX: t.fixedX,
		fixedY: t.fixedY
	});
	let m = /* @__PURE__ */ new Map(), h = /* @__PURE__ */ new Map(), g = (e, t) => {
		let n = m.get(e);
		n ? n.push(t) : m.set(e, [t]);
	}, _ = /* @__PURE__ */ new Set();
	for (let e of t) {
		let t = `${e.from}->${e.to}`;
		if (_.has(t)) continue;
		_.add(t);
		let n = d.get(e.from), r = d.get(e.to);
		if (n === void 0 || r === void 0 || r <= n) continue;
		if (r - n === 1) {
			g(e.from, e.to);
			continue;
		}
		let i = e.from, a = [];
		for (let o = n + 1; o < r; o++) {
			let n = `\u0000dummy:${t}:${o}`;
			p.set(n, {
				id: n,
				dummy: !0,
				layer: o,
				ownerId: e.from,
				edgeKey: t,
				w: 0,
				h: 0,
				order: p.size
			}), g(i, n), a.push(n), i = n;
		}
		g(i, e.to), a.length > 0 ? h.set(t, a.map((e) => ({
			x: 0,
			y: 0
		}))) : h.delete(t);
	}
	let ee = Math.max(0, ...[...p.values()].map((e) => e.layer + 1)), v = Array.from({ length: ee }, () => []);
	for (let e of p.values()) e.layer >= v.length && (v.length = e.layer + 1), v[e.layer] || (v[e.layer] = []), v[e.layer].push(e);
	let y = /* @__PURE__ */ new Map();
	for (let [e, t] of m) for (let n of t) {
		let t = y.get(n);
		t ? t.push(e) : y.set(n, [e]);
	}
	{
		let e = /* @__PURE__ */ new Map();
		for (let [, t] of m) t.forEach((t, n) => {
			e.has(t) || e.set(t, n);
		});
		let t = /* @__PURE__ */ new Map(), n = 0;
		for (let r of v) {
			let i = r.map((n) => {
				let r = y.get(n.id) ?? [], i = 2 ** 53 - 1;
				for (let e of r) {
					let n = t.get(e);
					n !== void 0 && n < i && (i = n);
				}
				return {
					node: n,
					parentRank: i,
					decl: e.get(n.id) ?? 0
				};
			});
			i.sort((e, t) => e.parentRank === t.parentRank ? e.decl === t.decl ? e.node.id < t.node.id ? -1 : +(e.node.id > t.node.id) : e.decl - t.decl : e.parentRank - t.parentRank), i.forEach((e, i) => {
				r[i] = e.node, t.set(e.node.id, n++);
			});
		}
	}
	let te = (e) => v[e].map((e) => e.id), b = (e, t, n) => {
		let r = e.map((e, r) => {
			let i = (t.get(e.id) ?? []).map((e) => n.get(e)).filter((e) => e !== void 0);
			return {
				node: e,
				idx: r,
				bary: i.length === 0 ? r : i.reduce((e, t) => e + t, 0) / i.length
			};
		});
		return r.sort((e, t) => e.bary === t.bary ? e.idx - t.idx : e.bary - t.bary), r.map((e) => e.node);
	}, x = (e) => {
		let t = /* @__PURE__ */ new Map();
		return e.forEach((e, n) => t.set(e.id, n)), t;
	}, ne = (e) => {
		let t = 0;
		return e > 0 && (t += M(te(e - 1), te(e), m)), e + 1 < v.length && (t += M(te(e), te(e + 1), m)), t;
	}, S = (e) => {
		let t = /* @__PURE__ */ new Map();
		e > 0 && v[e - 1].forEach((e, n) => t.set(e.id, n)), v[e].forEach((e, n) => t.set(e.id, n)), e + 1 < v.length && v[e + 1].forEach((e, n) => t.set(e.id, n));
		let n = 0, r = (e, r) => {
			let i = t.get(e), a = t.get(r);
			i !== void 0 && a !== void 0 && (n += Math.abs(i - a));
		};
		if (e > 0) for (let [n, i] of m) for (let a of i) t.has(n) && v[e].some((e) => e.id === a) && r(n, a);
		if (e + 1 < v.length) for (let t of v[e + 1]) for (let e of y.get(t.id) ?? []) r(e, t.id);
		return n;
	}, C = () => v.map((e, t) => te(t)), re = (e) => {
		for (let t = 0; t < v.length && t < e.length; t++) {
			let n = new Map(e[t].map((e, t) => [e, t]));
			v[t].sort((e, t) => (n.get(e.id) ?? 0) - (n.get(t.id) ?? 0));
		}
	}, ie = C(), w = P(ie, m);
	for (let e = 0; e < s && w > 0; e++) {
		for (let e = 1; e < v.length; e++) v[e] = b(v[e], y, x(v[e - 1]));
		for (let e = v.length - 2; e >= 0; e--) v[e] = b(v[e], m, x(v[e + 1]));
		for (let e = 0; e < v.length; e++) {
			let t = !0, n = v[e].length * 2 + 8;
			for (; t && n-- > 0;) {
				t = !1;
				for (let n = 0; n + 1 < v[e].length; n++) {
					let r = ne(e), i = S(e);
					[v[e][n], v[e][n + 1]] = [v[e][n + 1], v[e][n]];
					let a = ne(e), o = S(e);
					a < r || a === r && o < i ? t = !0 : [v[e][n], v[e][n + 1]] = [v[e][n + 1], v[e][n]];
				}
			}
		}
		let e = C(), t = P(e, m);
		if (t < w) w = t, ie = e;
		else break;
	}
	re(ie);
	let ae = (e) => e.dummy ? 0 : e.w, T = v.map((e) => e.filter((e) => !e.dummy)), E = /* @__PURE__ */ new Map(), D = (e) => {
		let t = p.get(e);
		return (E.get(e) ?? 0) + ae(t) / 2;
	};
	for (let e of T) {
		let t = 0;
		for (let n of e) {
			if (n.fixedX !== void 0) {
				E.set(n.id, n.fixedX), t = Math.max(t, n.fixedX + n.w + a);
				continue;
			}
			E.set(n.id, t), t += n.w + a;
		}
	}
	let oe = /* @__PURE__ */ new Map(), se = /* @__PURE__ */ new Map();
	for (let e of t) {
		if (e.from === e.to || !p.has(e.from) || !p.has(e.to)) continue;
		let t = oe.get(e.from);
		t ? t.push(e.to) : oe.set(e.from, [e.to]);
		let n = se.get(e.to);
		n ? n.push(e.from) : se.set(e.to, [e.from]);
	}
	let O = e.some((e) => e.fixedX !== void 0 || e.fixedY !== void 0), ce = .15, le = [];
	for (let e of T) for (let t of e) {
		let e = (oe.get(t.id) ?? []).filter((e) => E.has(e) && p.get(e)?.dummy === !1);
		e.length > 0 && le.push({
			node: t,
			kids: e
		});
	}
	let ue = [];
	for (let [e, t] of oe) for (let n of t) e !== n && E.has(e) && E.has(n) && ue.push([e, n]);
	let de = () => {
		let e = 0;
		for (let { node: t, kids: n } of le) {
			let r = 0;
			for (let e of n) r += D(e);
			r /= n.length;
			let i = D(t.id) - r;
			e += 1 * i * i;
		}
		for (let [t, n] of ue) {
			let r = D(t) - D(n);
			e += ce * r * r;
		}
		return e;
	}, fe = () => {
		let e = /* @__PURE__ */ new Map(), t = (t, n) => e.set(t, (e.get(t) ?? 0) + n);
		for (let { node: e, kids: n } of le) {
			let r = 0;
			for (let e of n) r += D(e);
			r /= n.length;
			let i = 2 * (D(e.id) - r);
			t(e.id, i);
			for (let e of n) t(e, -i / n.length);
		}
		for (let [e, n] of ue) {
			let r = 2 * ce * (D(e) - D(n));
			t(e, r), t(n, -r);
		}
		return e;
	}, pe = () => {
		for (let e of T) {
			let t = Infinity;
			for (let n = e.length - 1; n >= 0; n--) {
				let r = e[n];
				if (r.fixedX !== void 0) {
					t = r.fixedX - a;
					continue;
				}
				let i = E.get(r.id);
				if (Number.isFinite(t)) {
					let e = Math.min(i, t - r.w);
					E.set(r.id, e), t = e - a;
				} else t = i - a;
			}
			t = -Infinity;
			for (let n of e) {
				if (n.fixedX !== void 0) {
					t = n.fixedX + n.w + a;
					continue;
				}
				let e = Math.max(E.get(n.id), t);
				E.set(n.id, e), t = e + n.w + a;
			}
		}
	};
	{
		let e = .1, t = de(), n = 0;
		for (; n < 500; n++) {
			let n = fe(), r = !1, i = e, a = 0;
			for (let o = 0; o < 24 && !r; o++) {
				let o = new Map(E);
				a = 0;
				for (let e of T) for (let t of e) {
					if (t.fixedX !== void 0) continue;
					let e = n.get(t.id);
					if (!e) continue;
					let r = E.get(t.id), o = r - i * e;
					a = Math.max(a, Math.abs(o - r)), E.set(t.id, o);
				}
				pe();
				let s = de();
				if (s < t - 1e-9) r = !0, t = s, e = Math.min(i * 1.5, 1);
				else {
					for (let [e, t] of o) E.set(e, t);
					i *= .5;
				}
			}
			if (!r || a < .001) break;
		}
	}
	{
		let e = /* @__PURE__ */ new Map();
		for (let { node: t, kids: n } of le) e.set(t.id, n);
		for (let t = T.length - 2; t >= 0; t--) {
			for (let n of T[t]) {
				if (n.fixedX !== void 0) continue;
				let t = e.get(n.id);
				if (!t || t.length === 0) continue;
				let r = t.reduce((e, t) => e + D(t), 0) / t.length;
				E.set(n.id, r - n.w / 2);
			}
			pe();
		}
	}
	let k = [], A = [], me = 0;
	for (let e = 0; e < T.length; e++) {
		let t = T[e];
		A.push(k.length);
		let n = l >= 1 ? Math.floor(l) : Infinity;
		if (!Number.isFinite(n) || t.length <= n) {
			k.push(t);
			continue;
		}
		me++;
		let r = Math.ceil(t.length / n), i = Math.ceil(t.length / r), o = Math.min(...t.map((e) => E.get(e.id) ?? 0));
		for (let e = 0; e < t.length; e += i) {
			let n = t.slice(e, e + i), r = o;
			for (let e of n) {
				if (e.fixedX !== void 0) {
					r = Math.max(r, e.fixedX + e.w + a);
					continue;
				}
				E.set(e.id, r), r += e.w + a;
			}
			k.push(n);
		}
	}
	if (me > 0) {
		let e = (e) => {
			let t = Infinity;
			for (let n of e) t = Math.min(t, E.get(n.id) ?? 0);
			return t;
		}, t = (e) => {
			let t = -Infinity;
			for (let n of e) t = Math.max(t, (E.get(n.id) ?? 0) + n.w);
			return t;
		};
		for (let e of k) {
			let t = 0;
			for (let n of e) {
				if (n.fixedX !== void 0) {
					t = Math.max(t, n.fixedX + n.w + a);
					continue;
				}
				E.set(n.id, t), t += n.w + a;
			}
		}
		for (let n = 0; n < 3; n++) for (let n = 1; n < k.length; n++) {
			let r = k[n];
			if (r.length === 0) continue;
			let i = 0, a = 0;
			for (let e of r) for (let t of se.get(e.id) ?? []) E.has(t) && (i += (E.get(t) ?? 0) + ae(p.get(t)) / 2, a++);
			if (a === 0) continue;
			let o = i / a - (e(r) + t(r)) / 2;
			for (let e of r) e.fixedX === void 0 && E.set(e.id, (E.get(e.id) ?? 0) + o);
		}
	}
	let j = [], he = [], ge = 0;
	for (let e = 0; e < k.length; e++) {
		let t = k[e], n = t.length > 0 ? Math.max(...t.map((e) => e.h)) : 0;
		j.push(ge), he.push(n), ge += n + o;
	}
	let _e = /* @__PURE__ */ new Map();
	for (let e = 0; e < k.length; e++) for (let t of k[e]) {
		if (t.fixedY !== void 0) {
			_e.set(t.id, t.fixedY);
			continue;
		}
		_e.set(t.id, j[e] + (he[e] - t.h) / 2);
	}
	let ve = 0;
	if (!O) {
		let e = Infinity, t = Infinity;
		for (let n of p.values()) n.dummy || (e = Math.min(e, E.get(n.id) ?? Infinity), t = Math.min(t, _e.get(n.id) ?? Infinity));
		let n = Number.isFinite(e) ? i - e : 0, r = Number.isFinite(t) ? i - t : 0;
		ve = r;
		for (let e of p.values()) e.dummy || (E.set(e.id, (E.get(e.id) ?? 0) + n), _e.set(e.id, (_e.get(e.id) ?? 0) + r));
	}
	if (c) {
		let e = [];
		for (let t of p.values()) t.dummy || t.fixedX !== void 0 && t.fixedY !== void 0 && e.push({
			x: t.fixedX,
			y: t.fixedY,
			w: t.w,
			h: t.h
		});
		if (e.length > 0) {
			let t = (t, n, r, i) => e.some((e) => t < e.x + e.w && e.x < t + r && n < e.y + e.h && e.y < n + i);
			for (let n of p.values()) {
				if (n.dummy || n.fixedX !== void 0) continue;
				let r = E.get(n.id), i = _e.get(n.id);
				for (let e = 0; e < 4096 && t(r, i, n.w, n.h); e++) r += a;
				E.set(n.id, r), e.push({
					x: r,
					y: i,
					w: n.w,
					h: n.h
				});
			}
		}
	}
	let ye = /* @__PURE__ */ new Map();
	for (let e of p.values()) e.dummy || ye.set(e.id, {
		x: Math.round(E.get(e.id) ?? 0),
		y: Math.round(_e.get(e.id) ?? 0)
	});
	for (let [e, t] of h) {
		let [n, r] = e.split("->"), i = ye.get(n), a = ye.get(r), s = p.get(n), c = p.get(r);
		if (!i || !a || !s || !c) continue;
		let l = i.x + s.w / 2, u = a.x + c.w / 2, f = d.get(n) ?? 0;
		t.forEach((e, n) => {
			let r = (n + 1) / (t.length + 1), i = A[f + n + 1], a = i !== void 0 && j[i] !== void 0 ? j[i] - o / 2 + ve : 0;
			t[n] = {
				x: Math.round((l * (1 - r) + u * r) * 100) / 100,
				y: Math.round(a * 100) / 100
			};
		});
	}
	return {
		positions: ye,
		layers: d,
		routes: h,
		crossings: w,
		roots: f
	};
}
var Ee = /* @__PURE__ */ new Set([
	"id",
	"title",
	"x",
	"y",
	"w",
	"h",
	"link",
	"group"
]);
function F(e) {
	return typeof e == "number" && Number.isFinite(e) ? e : void 0;
}
var I = {
	t: "top",
	r: "right",
	b: "bottom",
	l: "left"
}, L = {
	top: "t",
	right: "r",
	bottom: "b",
	left: "l"
}, De = /^([trbl_])>([trbl_])$/;
function Oe(e) {
	let t = e.split("|").map((e) => e.trim()), n = { ref: t[0] };
	for (let e of t.slice(1)) {
		let t = e.match(De);
		t ? (t[1] !== "_" && (n.fromSide = I[t[1]]), t[2] !== "_" && (n.toSide = I[t[2]])) : e !== "" && (n.label = e);
	}
	return n;
}
function ke(e) {
	let t = e.ref;
	if (e.label && (t += ` | ${e.label}`), e.fromSide || e.toSide) {
		let n = e.fromSide ? L[e.fromSide] : "_", r = e.toSide ? L[e.toSide] : "_";
		t += ` | ${n}>${r}`;
	}
	return t;
}
var R = /^(.*)\s*@\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*$/;
function Ae(e) {
	let t = e.match(R);
	return !t || !t[1].trim() ? { name: e.trim() } : {
		name: t[1].trim(),
		geo: {
			x: Number(t[2]),
			y: Number(t[3]),
			w: Number(t[4]),
			h: Number(t[5])
		}
	};
}
function je(e) {
	let { name: t, x: n, y: r, w: i, h: a } = e;
	return [
		n,
		r,
		i,
		a
	].every((e) => typeof e == "number" && Number.isFinite(e)) ? `${t} @ ${n}, ${r}, ${i}, ${a}` : t;
}
function Me(e, t) {
	if (t) return { ...t };
	if (e.length === 0) return {
		x: 0,
		y: 0,
		w: 0,
		h: 0
	};
	let n = Infinity, r = Infinity, i = -Infinity, a = -Infinity;
	for (let t of e) n = Math.min(n, t.x), r = Math.min(r, t.y), i = Math.max(i, t.x + t.w), a = Math.max(a, t.y + t.h);
	return {
		x: n - 24,
		y: r - 34,
		w: i - n + 48,
		h: a - r + 34 + 24
	};
}
function Ne(e) {
	let t = null;
	for (let n of e.split("\n")) {
		let e = n.match(/^\s*(`{3,}|~{3,})/);
		if (e) {
			t === null ? t = e[1][0] : e[1][0] === t && (t = null);
			continue;
		}
		if (t !== null) continue;
		let r = n.match(/^#[ \t]+(.+?)\s*#*\s*$/);
		if (r) return r[1].trim();
	}
}
function Pe(e) {
	return /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(e.trim());
}
function Fe(e) {
	let t = e.trim();
	t.startsWith("<") && t.endsWith(">") && (t = t.slice(1, -1).trim());
	let n = t.search(/[?#]/);
	n !== -1 && (t = t.slice(0, n));
	try {
		t = decodeURI(t);
	} catch {}
	let r = [];
	for (let e of t.replace(/\\/g, "/").split("/")) if (!(e === "" || e === ".")) {
		if (e === "..") {
			r.pop();
			continue;
		}
		r.push(e);
	}
	return r.join("/");
}
function Ie(e) {
	return e.toLowerCase().replace(/[^\p{L}\p{N}\s-]/gu, "").replace(/[\s-]+/g, "-").replace(/^-+|-+$/g, "");
}
function Le(e) {
	return e.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1").replace(/\[([^\]]*)\]\([^)]*\)/g, "$1").replace(/[`*_~]/g, "").replace(/<[^>]+>/g, "").trim();
}
function Re(e) {
	let t = [], n = null;
	for (let r of e.split("\n")) {
		let e = r.match(/^\s*(`{3,}|~{3,})/);
		if (e) {
			n === null ? n = e[1][0] : e[1][0] === n && (n = null);
			continue;
		}
		if (n !== null) continue;
		let i = r.match(/^##[ \t]+(.+?)\s*#*\s*$/);
		if (i) {
			let e = Le(i[1]);
			e && t.push({
				anchor: Ie(e),
				title: e
			});
		}
	}
	return t.length >= 2 ? t : [];
}
function ze(e, t, n) {
	return we(e.map((e) => e.id), t.map((e) => ({
		from: e.from,
		to: e.to
	})), n);
}
function Be(e, t, n, r) {
	if (e.filter((e) => {
		let t = r.get(e.id);
		return t.rawX === void 0 || t.rawY === void 0;
	}).length === 0) return;
	let i = Te(e.map((e) => {
		let t = r.get(e.id);
		return {
			id: e.id,
			w: e.w,
			h: e.h,
			fixedX: t.rawX,
			fixedY: t.rawY
		};
	}), t.map((e) => ({
		from: e.from,
		to: e.to
	})), { avoidFixed: !0 });
	for (let t of e) {
		let e = i.positions.get(t.id);
		if (!e) continue;
		let n = r.get(t.id);
		n.rawX === void 0 && (t.x = e.x), n.rawY === void 0 && (t.y = e.y);
	}
}
function Ve(e) {
	let t = Object.keys(e).sort(), n = [], r = /* @__PURE__ */ new Map(), i = /* @__PURE__ */ new Map(), a = /* @__PURE__ */ new Map();
	for (let o of t) {
		let { params: t, body: s } = ge(e[o]), c = typeof t.id == "string" && t.id.trim() !== "" ? t.id.trim() : o.replace(/\.md$/, ""), l = typeof t.title == "string" && t.title.trim() !== "" && t.title.trim() || Ne(s) || c, u = Re(s), d = F(t.w) ?? 220, f = F(t.h) ?? 96, p = {};
		for (let [e, n] of Object.entries(t)) Ee.has(e) || (p[e] = n);
		let m = {
			id: c,
			title: l,
			docPath: o,
			depth: 0,
			blocks: u,
			x: F(t.x) ?? 0,
			y: F(t.y) ?? 0,
			w: d,
			h: f,
			attrs: p
		};
		if (i.set(c, {
			rawX: F(t.x),
			rawY: F(t.y)
		}), t.group !== void 0) {
			let e = (Array.isArray(t.group) ? t.group : [t.group]).map((e) => typeof e == "string" ? e : typeof e == "number" ? String(e) : "").filter((e) => e.trim() !== "");
			if (e.length > 1 && n.push(`文档 "${c}" 声明了多个 group，仅取第一个 "${Ae(e[0]).name || e[0]}"`), e.length > 0) {
				let { name: t, geo: n } = Ae(e[0]);
				t && a.set(c, {
					name: t,
					geo: n
				});
			}
		}
		r.has(c) && n.push(`重复 id "${c}"：${r.get(c).docPath} 被 ${o} 覆盖`), r.set(c, m);
	}
	let o = [...r.values()], s = new Map(o.map((e) => [e.docPath, e])), c = [], l = /* @__PURE__ */ new Set();
	function u(e) {
		if (e === "") return;
		let t = /\.md$/i.test(e) ? e : e + ".md";
		return r.get(e) ?? s.get(e) ?? s.get(t) ?? r.get(t.replace(/\.md$/i, ""));
	}
	function d(e, t) {
		let n = u(Fe(e));
		if (n) return n;
		if (!t) return;
		let r = t.includes("/") ? t.slice(0, t.lastIndexOf("/") + 1) : "";
		return r === "" ? void 0 : u(r + Fe(e));
	}
	function f(e, t, r, i) {
		let a = d(e), o = a ? d(t, a.docPath) : d(t);
		if (!a || !o) {
			let r = a ? t : e;
			n.push("连线 " + i + " 引用了不存在的文档 \"" + r + "\"");
			return;
		}
		if (a.id === o.id) {
			n.push("文档 \"" + a.id + "\" 不能连线自身");
			return;
		}
		let s = a.id + "->" + o.id;
		l.has(s) || (l.add(s), c.push({
			id: s,
			type: "link",
			from: a.id,
			to: o.id,
			label: r.label,
			fromSide: r.fromSide,
			toSide: r.toSide
		}));
	}
	for (let t of o) {
		let { params: n } = ge(e[t.docPath]);
		for (let e of _e(n.link)) {
			let { ref: n, label: r, fromSide: i, toSide: a } = Oe(e);
			n && (Pe(n) || f(t.id, n, {
				label: r,
				fromSide: i,
				toSide: a
			}, t.id + ".link"));
		}
	}
	let p = ze(o, c, n);
	for (let e of o) e.depth = p.get(e.id) ?? 0;
	Be(o, c, p, i);
	let m = /* @__PURE__ */ new Map();
	for (let e of o) {
		let t = a.get(e.id);
		if (!t) continue;
		let r = m.get(t.name);
		r || (r = { members: [] }, m.set(t.name, r)), r.members.push(e), t.geo && (r.geo ? (r.geo.x !== t.geo.x || r.geo.y !== t.geo.y || r.geo.w !== t.geo.w || r.geo.h !== t.geo.h) && n.push(`组 "${t.name}" 的显式几何被多个成员声明且不一致，取 ${r.holder} 的声明`) : (r.geo = t.geo, r.holder = e.docPath));
	}
	return {
		boxes: o,
		relations: c,
		groups: [...m.entries()].map(([e, t]) => ({
			name: e,
			members: t.members.map((e) => e.id),
			...Me(t.members, t.geo),
			explicit: t.geo !== void 0,
			holder: t.holder ?? t.members[0].docPath
		})),
		warnings: [...new Set(n)]
	};
}
function He(e, t) {
	switch (t) {
		case "top": return {
			x: e.x + e.w / 2,
			y: e.y,
			nx: 0,
			ny: -1
		};
		case "bottom": return {
			x: e.x + e.w / 2,
			y: e.y + e.h,
			nx: 0,
			ny: 1
		};
		case "left": return {
			x: e.x,
			y: e.y + e.h / 2,
			nx: -1,
			ny: 0
		};
		default: return {
			x: e.x + e.w,
			y: e.y + e.h / 2,
			nx: 1,
			ny: 0
		};
	}
}
function Ue(e, t) {
	let n = t.x + t.w / 2 - (e.x + e.w / 2), r = t.y + t.h / 2 - (e.y + e.h / 2);
	return Math.abs(r) >= Math.abs(n) ? r >= 0 ? {
		fromSide: "bottom",
		toSide: "top"
	} : {
		fromSide: "top",
		toSide: "bottom"
	} : n >= 0 ? {
		fromSide: "right",
		toSide: "left"
	} : {
		fromSide: "left",
		toSide: "right"
	};
}
function z(e, t) {
	let n = e.parallelCount ?? 1;
	if (n <= 1) return {
		x: 0,
		y: 0
	};
	let r = ((e.parallelIndex ?? 0) - (n - 1) / 2) * 14;
	return {
		x: -t.ny * r,
		y: t.nx * r
	};
}
var B = (e) => Math.round(e * 100) / 100, We = (e) => {
	let t = Ue(e.from, e.to), n = e.waypoints ?? [], r = (t, n) => {
		let r = He(e.from, t), i = He(e.to, n), a = z(e, r);
		return {
			p1: r,
			p2: i,
			start: {
				x: r.x + a.x,
				y: r.y + a.y
			},
			end: {
				x: i.x + a.x,
				y: i.y + a.y
			}
		};
	}, i = e.from.y + e.from.h <= e.to.y || e.to.y + e.to.h <= e.from.y, a = e.fromSide ?? (i ? "bottom" : t.fromSide), o = e.toSide ?? (i ? "top" : t.toSide), { p1: s, p2: c, start: l, end: u } = r(a, o), d = Ge(l, u, s, n).map((e) => ({
		x: e.x + (l.x - s.x),
		y: e.y + (l.y - s.y)
	}));
	d.length === 0 && n.length > 0 && ({p1: s, p2: c, start: l, end: u} = r(t.fromSide, t.toSide));
	let f = Math.hypot(u.x - l.x, u.y - l.y), p = Math.max(24, Math.min(f * .45, 96)), m, h;
	d.length === 0 ? (h = [l, u], m = `M ${B(l.x)} ${B(l.y)} C ${B(l.x + s.nx * p)} ${B(l.y + s.ny * p)}, ${B(u.x + c.nx * p)} ${B(u.y + c.ny * p)}, ${B(u.x)} ${B(u.y)}`) : (h = [
		l,
		...d,
		u
	], m = V(h, s, c));
	let g = Ke(h);
	return {
		d: m,
		x1: B(l.x),
		y1: B(l.y),
		x2: B(u.x),
		y2: B(u.y),
		labelX: B(g.x),
		labelY: B(g.y) - 7,
		fromSide: d.length === 0 && n.length > 0 ? t.fromSide : a,
		toSide: d.length === 0 && n.length > 0 ? t.toSide : o,
		points: h
	};
};
function Ge(e, t, n, r) {
	if (r.length === 0) return r;
	let i = {
		x: t.x - e.x,
		y: t.y - e.y
	}, a = Math.hypot(i.x, i.y) > 1;
	return r.filter((t) => {
		let r = t.x - e.x, o = t.y - e.y;
		return n.nx * r + n.ny * o < -2 ? !1 : a ? i.x * r + i.y * o > 0 : !0;
	});
}
function V(e, t, n) {
	if (e.length === 0) return "";
	if (e.length === 1) return `M ${B(e[0].x)} ${B(e[0].y)}`;
	let r = `M ${B(e[0].x)} ${B(e[0].y)}`;
	for (let i = 1; i < e.length; i++) {
		let a = e[i - 1], o = e[i], s = o.y - a.y, c = Math.max(14, Math.min(Math.abs(s) * .5, 90)), l = s >= 0 ? 1 : -1, u = Math.max(16, Math.min(c, 56)), d = i === 1 ? {
			x: a.x + t.nx * u,
			y: a.y + t.ny * u
		} : {
			x: a.x,
			y: a.y + l * c
		}, f = i === e.length - 1 ? {
			x: o.x + n.nx * u,
			y: o.y + n.ny * u
		} : {
			x: o.x,
			y: o.y - l * c
		};
		r += ` C ${B(d.x)} ${B(d.y)}, ${B(f.x)} ${B(f.y)}, ${B(o.x)} ${B(o.y)}`;
	}
	return r;
}
function Ke(e) {
	if (e.length === 0) return {
		x: 0,
		y: 0
	};
	if (e.length === 1) return e[0];
	let t = [], n = 0;
	for (let r = 1; r < e.length; r++) {
		let i = Math.hypot(e[r].x - e[r - 1].x, e[r].y - e[r - 1].y);
		t.push({
			a: e[r - 1],
			b: e[r],
			len: i
		}), n += i;
	}
	if (n === 0) return e[0];
	let r = n / 2;
	for (let e of t) {
		if (r <= e.len || e === t[t.length - 1]) {
			let t = e.len === 0 ? 0 : r / e.len;
			return {
				x: e.a.x + (e.b.x - e.a.x) * t,
				y: e.a.y + (e.b.y - e.a.y) * t
			};
		}
		r -= e.len;
	}
	return e[e.length - 1];
}
var qe = 28, Je = (e) => {
	let t = Ue(e.from, e.to), n = e.fromSide ?? t.fromSide, r = e.toSide ?? t.toSide, i = He(e.from, n), a = He(e.to, r), o = z(e, i), s = {
		x: i.x + o.x,
		y: i.y + o.y
	}, c = {
		x: a.x + o.x,
		y: a.y + o.y
	}, l = {
		x: s.x + i.nx * qe,
		y: s.y + i.ny * qe
	}, u = {
		x: c.x + a.nx * qe,
		y: c.y + a.ny * qe
	}, d = [s, l];
	for (let t of e.waypoints ?? []) d.push({
		x: t.x + o.x,
		y: t.y + o.y
	});
	d.push(u, c);
	let f = [d[0]], p = i.nx !== 0;
	for (let e = 1; e < d.length; e++) {
		let t = f[f.length - 1], n = d[e];
		if (Math.abs(t.x - n.x) > .5 && Math.abs(t.y - n.y) > .5) {
			let e = p ? {
				x: n.x,
				y: t.y
			} : {
				x: t.x,
				y: n.y
			};
			f.push(e);
		}
		f.push(n);
	}
	let m = Ye(f), h = Ke(m);
	return {
		d: Xe(m, 10),
		x1: B(s.x),
		y1: B(s.y),
		x2: B(c.x),
		y2: B(c.y),
		labelX: B(h.x),
		labelY: B(h.y) - 7,
		fromSide: n,
		toSide: r,
		points: m
	};
};
function Ye(e) {
	let t = [];
	for (let n of e) {
		let e = t[t.length - 1];
		e && Math.abs(e.x - n.x) < .5 && Math.abs(e.y - n.y) < .5 || t.push(n);
	}
	if (t.length < 3) return t;
	let n = [t[0]];
	for (let e = 1; e < t.length - 1; e++) {
		let r = n[n.length - 1], i = t[e], a = t[e + 1], o = Math.abs(r.x - i.x) < .5 && Math.abs(i.x - a.x) < .5, s = Math.abs(r.y - i.y) < .5 && Math.abs(i.y - a.y) < .5;
		!o && !s && n.push(i);
	}
	return n.push(t[t.length - 1]), n;
}
function Xe(e, t) {
	if (e.length === 0) return "";
	if (e.length === 1) return `M ${B(e[0].x)} ${B(e[0].y)}`;
	let n = `M ${B(e[0].x)} ${B(e[0].y)}`;
	for (let r = 1; r < e.length; r++) {
		let i = e[r - 1], a = e[r];
		if (r === e.length - 1) {
			n += ` L ${B(a.x)} ${B(a.y)}`;
			continue;
		}
		let o = e[r + 1], s = Math.hypot(a.x - i.x, a.y - i.y), c = Math.hypot(o.x - a.x, o.y - a.y), l = Math.min(t, s / 2, c / 2);
		if (l < 1) {
			n += ` L ${B(a.x)} ${B(a.y)}`;
			continue;
		}
		let u = {
			x: a.x - (a.x - i.x) / s * l,
			y: a.y - (a.y - i.y) / s * l
		}, d = {
			x: a.x + (o.x - a.x) / c * l,
			y: a.y + (o.y - a.y) / c * l
		};
		n += ` L ${B(u.x)} ${B(u.y)} Q ${B(a.x)} ${B(a.y)}, ${B(d.x)} ${B(d.y)}`;
	}
	return n;
}
function Ze(e, t, n, r) {
	let i = n.x - r, a = n.x + n.w + r, o = n.y - r, s = n.y + n.h + r, c = Math.min(e.x, t.x), l = Math.max(e.x, t.x), u = Math.min(e.y, t.y), d = Math.max(e.y, t.y);
	return u === d ? u > o && u < s && l > i && c < a : c === l ? c > i && c < a && d > o && u < s : !1;
}
var Qe = {
	bezier: We,
	orthogonal: Je,
	avoid: (e) => {
		let t = (e.obstacles ?? []).filter((t) => !(t.x === e.from.x && t.y === e.from.y && t.w === e.from.w && t.h === e.from.h) && !(t.x === e.to.x && t.y === e.to.y && t.w === e.to.w && t.h === e.to.h));
		if (t.length === 0) return Je(e);
		let n = Ue(e.from, e.to), r = e.fromSide ?? n.fromSide, i = e.toSide ?? n.toSide, a = He(e.from, r), o = He(e.to, i), s = {
			x: a.x,
			y: a.y
		}, c = {
			x: o.x,
			y: o.y
		}, l = /* @__PURE__ */ new Set([s.x, c.x]), u = /* @__PURE__ */ new Set([s.y, c.y]);
		for (let e of t) l.add(e.x - 18), l.add(e.x + e.w + 18), u.add(e.y - 18), u.add(e.y + e.h + 18);
		let d = [...l].sort((e, t) => e - t), f = [...u].sort((e, t) => e - t), p = new Map(d.map((e, t) => [e, t])), m = new Map(f.map((e, t) => [e, t])), h = (e, t) => t * d.length + e, g = d.length * f.length;
		if (g > 4e4) return Je(e);
		let _ = (e, n) => !t.some((t) => Ze(e, n, t, 6)), ee = p.get(s.x), v = m.get(s.y), y = p.get(c.x), te = m.get(c.y), b = new Float64Array(g).fill(Infinity), x = new Float64Array(g).fill(Infinity), ne = new Int32Array(g).fill(-1), S = new Uint8Array(g), C = h(ee, v), re = h(y, te), ie = (e, t) => Math.abs(d[e] - c.x) + Math.abs(f[t] - c.y);
		b[C] = 0, x[C] = ie(ee, v);
		let w = [C], ae = (e, t) => {
			let n = [];
			return e > 0 && n.push([e - 1, t]), e + 1 < d.length && n.push([e + 1, t]), t > 0 && n.push([e, t - 1]), t + 1 < f.length && n.push([e, t + 1]), n;
		}, T = !1;
		for (; w.length > 0;) {
			let e = 0;
			for (let t = 1; t < w.length; t++) x[w[t]] < x[w[e]] && (e = t);
			let t = w.splice(e, 1)[0];
			if (t === re) {
				T = !0;
				break;
			}
			if (S[t]) continue;
			S[t] = 1;
			let n = t % d.length, r = (t - n) / d.length, i = {
				x: d[n],
				y: f[r]
			};
			for (let [e, a] of ae(n, r)) {
				let n = h(e, a);
				if (S[n]) continue;
				let r = {
					x: d[e],
					y: f[a]
				};
				if (!_(i, r)) continue;
				let o = Math.abs(r.x - i.x) + Math.abs(r.y - i.y), s = b[t] + o;
				s >= b[n] || (b[n] = s, x[n] = s + ie(e, a), ne[n] = t, w.push(n));
			}
		}
		if (!T) return Je(e);
		let E = [];
		for (let e = re; e !== -1; e = ne[e]) {
			let t = e % d.length, n = (e - t) / d.length;
			if (E.push({
				x: d[t],
				y: f[n]
			}), e === C) break;
		}
		E.reverse();
		let D = Ye(E), oe = Ke(D);
		return {
			d: Xe(D, 10),
			x1: B(s.x),
			y1: B(s.y),
			x2: B(c.x),
			y2: B(c.y),
			labelX: B(oe.x),
			labelY: B(oe.y) - 7,
			fromSide: r,
			toSide: i,
			points: D
		};
	}
};
function $e(e) {
	return e ? Qe[e] ?? We : We;
}
var et = /^[a-z][a-z0-9_-]*$/;
function tt(e) {
	let t = [], n = /* @__PURE__ */ new Set();
	for (let r of e) {
		if (!r || typeof r.name != "string" || r.name.trim() === "") {
			t.push("插件缺少 name，已忽略");
			continue;
		}
		n.has(r.name) && t.push(`插件 "${r.name}" 重复注册，后注册者生效`), n.add(r.name);
		for (let e of r.nodeTypes ?? []) if (et.test(e.name) || t.push(`插件 "${r.name}" 的节点类型名 "${e.name}" 不合法（需小写字母开头，仅含小写字母/数字/-/_）`), e.defaultSize) {
			let { w: n, h: i } = e.defaultSize;
			(!Number.isFinite(n) || !Number.isFinite(i) || n <= 0 || i <= 0) && t.push(`插件 "${r.name}" 的节点类型 "${e.name}" 默认尺寸非法，已忽略`);
		}
	}
	return t;
}
function nt(e) {
	let t = e ?? [], n = tt(t), r = /* @__PURE__ */ new Map(), i, a;
	for (let e of t) if (!(!e || typeof e.name != "string")) {
		for (let t of e.nodeTypes ?? []) {
			if (!t || typeof t.name != "string" || !et.test(t.name)) continue;
			let e = t.defaultSize, n = e && Number.isFinite(e.w) && Number.isFinite(e.h) && e.w > 0 && e.h > 0 ? e : void 0;
			r.set(t.name, {
				...t,
				defaultSize: n
			});
		}
		!i && e.edgeRouter && (i = e.edgeRouter), !a && e.layout && (a = e.layout);
	}
	return {
		nodeTypes: r,
		edgeRouter: i,
		layout: a,
		warnings: n
	};
}
function rt(e) {
	let t = e?.type;
	if (typeof t != "string") return "doc";
	let n = t.trim().toLowerCase();
	return et.test(n) ? n : "doc";
}
function it(e, t, n) {
	return e.map((e) => {
		let r = rt(e.attrs), i = t.get(r), a = i?.defaultSize, o = e.w !== n.w, s = e.h !== n.h;
		return {
			...e,
			nodeType: r,
			connectable: i?.connectable ?? !0,
			hasExplicitSize: o || s,
			w: o || !a ? e.w : a.w,
			h: s || !a ? e.h : a.h
		};
	});
}
function at(e, t) {
	if (e !== "doc") return t.get(e)?.badge ?? e;
}
var ot = "\0group:";
function st(e) {
	return `${ot}${e}`;
}
function ct(e, t) {
	let n = /* @__PURE__ */ new Map(), r = [], i = [];
	for (let a of e.groups) {
		if (!t.has(a.name) || a.members.length === 0) {
			i.push(a);
			continue;
		}
		let e = st(a.name);
		r.push({
			id: e,
			name: a.name,
			x: a.x,
			y: a.y,
			w: a.w,
			h: a.h,
			memberIds: [...a.members],
			memberCount: a.members.length,
			title: `${a.name}（${a.members.length} 篇）`
		});
		for (let t of a.members) n.set(t, e);
	}
	if (r.length === 0) return {
		boxes: [...e.boxes],
		groupNodes: [],
		relations: [...e.relations],
		expandedGroups: i,
		redirect: n
	};
	let a = new Set(n.keys()), o = e.boxes.filter((e) => !a.has(e.id)), s = [], c = /* @__PURE__ */ new Set();
	for (let t of e.relations) {
		let e = n.get(t.from) ?? t.from, r = n.get(t.to) ?? t.to;
		if (e === r) continue;
		let i = `${e}->${r}`;
		c.has(i) || (c.add(i), s.push({
			...t,
			id: i,
			from: e,
			to: r,
			label: t.label
		}));
	}
	return {
		boxes: o,
		groupNodes: r,
		relations: s,
		expandedGroups: i,
		redirect: n
	};
}
var lt = {
	title: 1e3,
	path: 400,
	heading: 300,
	body: 20
}, ut = 2;
function dt(e) {
	return e.map((e) => ({
		docPath: e.docPath,
		id: e.id,
		title: e.title,
		body: e.body,
		blocks: (e.blocks ?? []).map((e) => ({ ...e })),
		lowerTitle: e.title.toLowerCase(),
		lowerPath: e.docPath.toLowerCase(),
		lowerBody: e.body.toLowerCase(),
		lowerBlocks: (e.blocks ?? []).map((e) => ({
			title: e.title,
			anchor: e.anchor,
			lower: e.title.toLowerCase()
		}))
	}));
}
function ft(e, t, n = 8) {
	if (t === "") return [];
	let r = [], i = 0;
	for (; r.length < n;) {
		let n = e.indexOf(t, i);
		if (n === -1) break;
		r.push(n), i = n + t.length;
	}
	return r;
}
function pt(e, t, n, r) {
	let i = ft(t, n);
	if (i.length === 0) return {
		snippet: e.slice(0, r * 2),
		ranges: []
	};
	let a = i[0], o = Math.max(0, a - r), s = Math.min(e.length, a + n.length + r), c = (o > 0 ? "…" : "") + e.slice(o, s) + (s < e.length ? "…" : ""), l = +(o > 0);
	return {
		snippet: c,
		ranges: i.filter((e) => e >= o && e + n.length <= s).map((e) => [e - o + l, e - o + n.length + l])
	};
}
function mt(e, t, n = {}) {
	let r = t.trim().toLowerCase();
	if (r === "") return [];
	let i = n.limit ?? 20, a = n.context ?? 40, o = n.includeBody ?? !0, s = [];
	for (let t of e) {
		if (t.lowerTitle.includes(r)) {
			let { snippet: e, ranges: n } = pt(t.title, t.lowerTitle, r, a), i = ft(t.lowerTitle, r).length;
			s.push({
				docPath: t.docPath,
				boxId: t.id,
				title: t.title,
				kind: "title",
				snippet: e,
				ranges: n,
				score: lt.title + i * 10
			});
		}
		t.lowerPath.includes(r) && !t.lowerTitle.includes(r) && s.push({
			docPath: t.docPath,
			boxId: t.id,
			title: t.title,
			kind: "path",
			snippet: t.docPath,
			ranges: ft(t.lowerPath, r).map((e) => [e, e + r.length]),
			score: lt.path
		});
		for (let e of t.lowerBlocks) e.lower.includes(r) && s.push({
			docPath: t.docPath,
			boxId: t.id,
			title: t.title,
			kind: "heading",
			snippet: e.title,
			ranges: ft(e.lower, r).map((e) => [e, e + r.length]),
			anchor: e.anchor,
			score: lt.heading + ft(e.lower, r).length * 5
		});
		if (o) {
			let e = ft(t.lowerBody, r, ut);
			for (let n of e) {
				let i = Math.max(0, n - a), o = Math.min(t.body.length, n + r.length + a), c = i > 0 ? "…" : "", l = c + t.body.slice(i, o) + (o < t.body.length ? "…" : ""), u = c.length;
				s.push({
					docPath: t.docPath,
					boxId: t.id,
					title: t.title,
					kind: "body",
					snippet: l,
					ranges: [[n - i + u, n - i + r.length + u]],
					score: lt.body + e.length
				});
			}
		}
	}
	return s.sort((e, t) => t.score === e.score ? e.docPath === t.docPath ? e.snippet < t.snippet ? -1 : +(e.snippet > t.snippet) : e.docPath < t.docPath ? -1 : 1 : t.score - e.score), s.slice(0, i);
}
function ht(e) {
	let t = [], n = /* @__PURE__ */ new Map();
	for (let r of e) {
		let e = n.get(r.boxId);
		e || (e = {
			boxId: r.boxId,
			docPath: r.docPath,
			title: r.title,
			hits: []
		}, n.set(r.boxId, e), t.push(r.boxId)), e.hits.push(r);
	}
	return t.map((e) => n.get(e));
}
//#endregion
//#region src/components/DocGraphViewer.vue?vue&type=script&setup=true&lang.ts
var gt = { class: "pd-graph-viewer" }, _t = { class: "pd-graph-header" }, vt = {
	key: 0,
	class: "pd-graph-current"
}, yt = { class: "pd-graph-actions" }, bt = ["disabled"], xt = ["disabled"], St = ["disabled"], Ct = ["disabled"], wt = ["title"], Tt = ["disabled"], Et = { class: "pd-warn-trigger" }, Dt = { class: "pd-warn-list" }, Ot = { class: "pd-graph-main" }, kt = [
	"title",
	"onPointerdown",
	"onClick",
	"onKeydown"
], At = ["aria-label", "onPointerdown"], jt = ["width", "height"], Mt = [
	"d",
	"onClick",
	"onDblclick"
], Nt = ["d"], Pt = ["x", "y"], Ft = ["d"], It = [
	"x1",
	"y1",
	"x2",
	"y2"
], Lt = [
	"aria-label",
	"title",
	"onClick",
	"onDblclick",
	"onKeydown"
], Rt = { class: "pd-group-node__name" }, zt = { class: "pd-group-node__count" }, Bt = [
	"aria-label",
	"onPointerdown",
	"onClick",
	"onKeydown",
	"onMouseenter"
], Vt = {
	key: 1,
	class: "pd-doc-box__head"
}, Ht = ["title"], Ut = {
	key: 0,
	class: "pd-doc-box__badge"
}, Wt = [
	"aria-label",
	"onClick",
	"onKeydown"
], Gt = [
	"aria-label",
	"title",
	"onClick"
], Kt = ["aria-label", "onPointerdown"], qt = {
	class: "pd-doc-blocks-pop__card",
	role: "menu"
}, Jt = [
	"title",
	"aria-label",
	"onClick",
	"onKeydown"
], Yt = ["aria-label", "onClick"], Xt = ["aria-label"], Zt = ["aria-label", "onKeydown"], Qt = ["width", "height"], $t = { class: "pd-edge-handle-grp" }, en = ["cx", "cy"], tn = ["cx", "cy"], nn = { class: "pd-edge-handle-grp" }, rn = ["cx", "cy"], an = ["cx", "cy"], on = {
	key: 2,
	class: "pd-edit-toolbar",
	role: "toolbar",
	"aria-label": "图编辑工具栏"
}, sn = { class: "pd-search-panel" }, cn = { class: "pd-search-head" }, ln = { class: "pd-search-body" }, un = {
	key: 0,
	class: "pd-search-empty"
}, dn = {
	key: 1,
	class: "pd-search-empty"
}, fn = {
	key: 2,
	class: "pd-search-results"
}, pn = ["onClick", "onMouseenter"], mn = { class: "pd-search-hit__head" }, hn = { class: "pd-search-hit__title" }, gn = { class: "pd-search-hit__kind" }, _n = { class: "pd-search-hit__snippet" }, vn = { key: 0 }, yn = { class: "pd-search-foot" }, bn = { class: "pd-conflict-reason" }, xn = {
	key: 0,
	class: "pd-conflict-grid"
}, Sn = { class: "pd-conflict-col" }, Cn = {
	key: 0,
	class: "pd-conflict-pre"
}, wn = {
	key: 1,
	class: "pd-conflict-pre"
}, Tn = { class: "pd-conflict-col pd-conflict-col--mine" }, En = { class: "pd-conflict-pre" }, Dn = {
	key: 1,
	class: "pd-conflict-tip"
}, On = 40, kn = 4, An = 30, jn = /* @__PURE__ */ c({
	__name: "DocGraphViewer",
	props: {
		files: {},
		saveHandler: { type: Function },
		deleteHandler: { type: Function },
		plugins: {},
		edgeRouter: {},
		nodeRenderers: {},
		collapsedGroups: {}
	},
	emits: ["navigate", "save"],
	setup(c, { emit: se }) {
		let O = c, le = se, fe = g(null), { theme: pe } = D();
		function j(e, t = "info", n = 4e3) {
			fe.value?.addToast({
				message: e,
				type: t,
				duration: n
			});
		}
		async function he(e, t, n) {
			if (O.saveHandler) {
				let r = await O.saveHandler(e, t, n);
				return typeof r == "object" ? r : { ok: r };
			}
			return le("save", e, t, n), { ok: !0 };
		}
		async function _e(e, t) {
			if (!O.deleteHandler) return {
				ok: !1,
				error: "当前环境不支持删除文档"
			};
			let n = await O.deleteHandler(e, t);
			return typeof n == "object" ? n : { ok: n };
		}
		function ye(e) {
			return e === void 0 ? {
				ok: !1,
				error: "未提供写处理器"
			} : typeof e == "boolean" ? { ok: e } : e;
		}
		async function be(e) {
			try {
				let t = await fetch(`/__prodoc_api/raw?path=${encodeURIComponent(e)}`);
				if (!t.ok) return null;
				let n = await t.json();
				return n.success && typeof n.content == "string" ? n.content : null;
			} catch {
				return null;
			}
		}
		function we(e, t, n, r) {
			if (n.status === 409) {
				let i = e === "保存" ? "与磁盘上的外部修改改到了同一区域，无法自动合并" : "在磁盘上已被其他程序修改";
				j(`「${t}」${i}，${e}被拒绝。可刷新页面同步后重试（或「↩ 放弃更改」丢弃）。`, "error", 8e3), Hn(t, e, n.error ?? i, r);
			} else j(`「${t}」${e}失败：${n.error ?? "未知错误"}`, "error", 6e3);
		}
		let M = g(/* @__PURE__ */ new Map()), N = g(/* @__PURE__ */ new Set()), P = /* @__PURE__ */ new Map(), Ee = t(() => M.value.size > 0 || N.value.size > 0), F = g([]), I = g([]);
		function L() {
			return {
				drafts: [...M.value],
				newPaths: [...Y],
				deletes: [...N.value],
				overrides: G.value ? [...G.value] : null,
				bases: [...P]
			};
		}
		function De(e, t) {
			if (e.deletes.length !== t.deletes.length || e.newPaths.length !== t.newPaths.length || e.drafts.length !== t.drafts.length || (e.overrides?.length ?? 0) !== (t.overrides?.length ?? 0)) return !1;
			let n = new Map(e.drafts);
			if (!t.drafts.every(([e, t]) => n.get(e) === t) || !e.deletes.every((e) => t.deletes.includes(e)) || !e.newPaths.every((e) => t.newPaths.includes(e))) return !1;
			let r = new Map(e.overrides ?? []);
			return (t.overrides ?? []).every(([e, t]) => r.get(e)?.x === t.x && r.get(e)?.y === t.y);
		}
		function R(e) {
			De(e, L()) || (F.value = [...F.value, e], I.value = []);
		}
		function Ae(e) {
			M.value = new Map(e.drafts), Y = new Set(e.newPaths), N.value = new Set(e.deletes), G.value = e.overrides ? new Map(e.overrides) : null, P.clear();
			for (let [t, n] of e.bases) P.set(t, n);
		}
		function Ne() {
			let e = F.value[F.value.length - 1];
			e && (F.value = F.value.slice(0, -1), I.value = [...I.value, L()], Ae(e));
		}
		function Pe() {
			let e = I.value[I.value.length - 1];
			e && (I.value = I.value.slice(0, -1), F.value = [...F.value, L()], Ae(e));
		}
		let Fe = `prodoc-drafts:${typeof location < "u" ? location.origin : ""}`;
		function Le() {
			try {
				if (!Ee.value) {
					localStorage.removeItem(Fe);
					return;
				}
				localStorage.setItem(Fe, JSON.stringify({
					v: 1,
					drafts: [...M.value].map(([e, t]) => ({
						path: e,
						base: P.get(e) ?? O.files[e] ?? null,
						content: t
					})),
					deletes: [...N.value].map((e) => ({
						path: e,
						base: O.files[e] ?? null
					}))
				}));
			} catch {}
		}
		function Re() {
			let e = null;
			try {
				e = localStorage.getItem(Fe), localStorage.removeItem(Fe);
			} catch {
				return 0;
			}
			if (!e) return 0;
			try {
				let t = JSON.parse(e), n = /* @__PURE__ */ new Map(), r = /* @__PURE__ */ new Set();
				for (let e of t.drafts ?? []) {
					let t = O.files[e.path];
					t !== e.content && (e.base !== null && t !== e.base || e.base === null && t !== void 0 || (n.set(e.path, e.content), P.set(e.path, e.base), t === void 0 && Y.add(e.path)));
				}
				for (let e of t.deletes ?? []) {
					let t = O.files[e.path];
					t !== void 0 && (e.base !== null && t !== e.base || r.add(e.path));
				}
				let i = n.size + r.size;
				return i > 0 && (M.value = n, N.value = r), i;
			} catch {
				return 0;
			}
		}
		b([M, N], Le);
		let ze = t(() => M.value.size ? {
			...O.files,
			...Object.fromEntries(M.value)
		} : O.files), Be = t(() => nt(O.plugins)), He = t(() => O.edgeRouter ? $e(O.edgeRouter) : Be.value.edgeRouter ?? $e("bezier")), z = t(() => {
			let e = Ve(ze.value), t = Be.value, n = t.warnings;
			if (n.length === 0 && t.nodeTypes.size === 0) return e;
			let r = it(e.boxes, t.nodeTypes, {
				w: 220,
				h: 96
			});
			return {
				...e,
				boxes: r,
				warnings: [...e.warnings, ...n]
			};
		}), B = (e) => e.nodeType ?? "doc", We = (e) => at(B(e), Be.value.nodeTypes), Ge = (e) => O.nodeRenderers?.[B(e)], V = g(new Set(O.collapsedGroups ?? [])), Ke = t(() => ct({
			boxes: z.value.boxes,
			relations: z.value.relations,
			groups: z.value.groups
		}, V.value));
		function qe(e) {
			let t = new Set(V.value);
			t.has(e) ? t.delete(e) : t.add(e), V.value = t;
		}
		function Je() {
			if (V.value.size > 0) {
				V.value = /* @__PURE__ */ new Set();
				return;
			}
			V.value = new Set(z.value.groups.map((e) => e.name));
		}
		function Ye(e) {
			let t = sr.value.get(e)?.memberIds[0];
			if (t) {
				let e = z.value.boxes.find((e) => e.id === t);
				e && Gi(e.docPath);
			}
		}
		let Xe = t(() => Object.fromEntries(Object.entries(O.files).map(([e, t]) => [e, ge(t).body])));
		b(() => z.value.warnings, (e) => e.forEach((e) => console.warn("[ProDoc]", e)), { immediate: !0 });
		let Ze = g({
			panX: 0,
			panY: 0,
			zoom: 1
		});
		function Qe() {
			let e = Yn.value?.getView?.();
			if (!e) return;
			let t = Ze.value;
			Math.abs(t.panX - e.panX) < .5 && Math.abs(t.panY - e.panY) < .5 && Math.abs(t.zoom - e.zoom) < .001 || (Ze.value = {
				panX: e.panX,
				panY: e.panY,
				zoom: e.zoom
			});
		}
		let et = 0;
		function tt() {
			if (et) return;
			let e = () => {
				Qe(), et = requestAnimationFrame(e);
			};
			et = requestAnimationFrame(e);
		}
		function rt() {
			et &&= (cancelAnimationFrame(et), 0);
		}
		let ot = t(() => {
			let { panX: e, panY: t, zoom: n } = Ze.value, r = Yn.value?.getViewportRect?.() ?? null, i = r?.width ?? kr.value?.parentElement?.clientWidth ?? 0, a = r?.height ?? kr.value?.parentElement?.clientHeight ?? 0;
			if (!i || !a) return null;
			let o = n > 0 ? n : 1, s = 240 / o;
			return {
				x: -e / o - s,
				y: -t / o - s,
				w: i / o + s * 2,
				h: a / o + s * 2
			};
		}), st = t(() => {
			let e = ot.value;
			return !e || !ut.value ? K.value : K.value.filter((t) => t.x < e.x + e.w && e.x < t.x + t.w && t.y < e.y + e.h && e.y < t.y + t.h);
		}), lt = t(() => {
			let e = ot.value;
			return !e || !ut.value ? $n.value : $n.value.filter((t) => {
				let n = Math.min(t.x1, t.x2), r = Math.max(t.x1, t.x2), i = Math.min(t.y1, t.y2), a = Math.max(t.y1, t.y2);
				return n < e.x + e.w && e.x < r && i < e.y + e.h && e.y < a;
			});
		}), ut = t(() => K.value.length > On), ft = t(() => {
			let e = Ze.value.zoom;
			return e >= .6 ? "full" : e >= .35 ? "compact" : "outline";
		}), pt = g(!1), jn = g(""), Mn = g(0), Nn = t(() => dt(z.value.boxes.map((e) => ({
			docPath: e.docPath,
			id: e.id,
			title: e.title,
			body: Xe.value[e.docPath] ?? "",
			blocks: e.blocks
		})))), Pn = t(() => !pt.value || jn.value.trim() === "" ? [] : ht(mt(Nn.value, jn.value, { limit: 40 }))), Fn = t(() => Pn.value.flatMap((e) => e.hits));
		b(jn, () => {
			Mn.value = 0;
		}), b(pt, (e) => {
			e || (jn.value = "", Mn.value = 0);
		});
		function In() {
			pt.value = !0, u(() => Bn.value?.focus());
		}
		function Ln() {
			pt.value = !1;
		}
		function Rn(e) {
			if (Ln(), e.anchor) {
				qi(e.docPath, e.anchor);
				return;
			}
			Gi(e.docPath);
		}
		function zn(e) {
			let t = Fn.value.length;
			if (e.key === "Escape") {
				e.preventDefault(), Ln();
				return;
			}
			if (e.key === "ArrowDown" && t > 0) {
				e.preventDefault(), Mn.value = (Mn.value + 1) % t;
				return;
			}
			if (e.key === "ArrowUp" && t > 0) {
				e.preventDefault(), Mn.value = (Mn.value - 1 + t) % t;
				return;
			}
			if (e.key === "Enter") {
				let t = Fn.value[Mn.value];
				t && (e.preventDefault(), Rn(t));
			}
		}
		let Bn = g(null);
		function Vn(e) {
			let t = [], n = 0;
			for (let [r, i] of e.ranges) r > n && t.push({
				text: e.snippet.slice(n, r),
				mark: !1
			}), t.push({
				text: e.snippet.slice(r, i),
				mark: !0
			}), n = i;
			return n < e.snippet.length && t.push({
				text: e.snippet.slice(n),
				mark: !1
			}), t;
		}
		let H = g(null);
		async function Hn(e, t, n, r) {
			Un({
				path: e,
				action: t,
				error: n,
				mine: r ?? M.value.get(e) ?? "",
				disk: await be(e)
			});
		}
		function Un(e) {
			H.value = e;
		}
		function Wn() {
			H.value = null;
		}
		async function Gn() {
			let e = H.value;
			if (!e || !O.saveHandler) return;
			H.value = null;
			let t = ye(await O.saveHandler(e.path, e.mine, e.disk ?? void 0));
			if (t.ok) {
				j(`「${e.path}」已用你的版本覆盖`, "success"), qn(e.path);
				return;
			}
			j(`覆盖失败：${t.error ?? "未知错误"}`, "error", 6e3);
		}
		function Kn() {
			let e = H.value;
			e && (H.value = null, qn(e.path), j(`已放弃对「${e.path}」的修改，采用磁盘版本`, "info"));
		}
		function qn(e) {
			let t = new Map(M.value);
			if (t.delete(e), M.value = t, Y.delete(e), P.delete(e), N.value.has(e)) {
				let t = new Set(N.value);
				t.delete(e), N.value = t;
			}
		}
		let Jn = t(() => {
			let e = H.value;
			if (!e) return null;
			let t = e.mine.split("\n"), n = (e, t, n, r) => {
				let i = [];
				for (let a = t; a <= n && a < e.length; a++) i.push({
					no: a + 1,
					text: e[a],
					diff: a >= r[0] && a <= r[1]
				});
				return i;
			};
			if (e.disk === null) return {
				disk: null,
				mine: n(t, 0, Math.min(t.length - 1, 10), [0, t.length - 1]),
				diskTotal: 0,
				mineTotal: t.length,
				changed: t.length
			};
			let r = e.disk.split("\n"), i = Math.min(r.length, t.length), a = 0;
			for (; a < i && r[a] === t[a];) a++;
			let o = r.length - 1, s = t.length - 1;
			for (; o > a && s > a && r[o] === t[s];) o--, s--;
			let c = Math.max(0, a - kn), l = Math.min(r.length - 1, o + kn), u = Math.min(t.length - 1, s + kn);
			return {
				disk: n(r, c, l, [a, o]),
				mine: n(t, c, u, [a, s]),
				diskTotal: r.length,
				mineTotal: t.length,
				changed: Math.max(o - a + 1, s - a + 1)
			};
		}), U = g(null), Yn = g(null), Xn = t(() => {
			let e = 0, t = 0;
			for (let n of K.value) e = Math.max(e, n.x + n.w + 48), t = Math.max(t, n.y + n.h + 48);
			for (let n of fr.value) e = Math.max(e, n.x + n.w + 48), t = Math.max(t, n.y + n.h + 48);
			return {
				w: Math.max(e, 640),
				h: Math.max(t, 480)
			};
		}), Zn = t(() => {
			let e = Infinity, t = Infinity, n = -Infinity, r = -Infinity;
			for (let i of K.value) e = Math.min(e, i.x), t = Math.min(t, i.y), n = Math.max(n, i.x + i.w), r = Math.max(r, i.y + i.h);
			for (let i of fr.value) e = Math.min(e, i.x), t = Math.min(t, i.y), n = Math.max(n, i.x + i.w), r = Math.max(r, i.y + i.h);
			return Number.isFinite(e) ? {
				x: e - 48,
				y: t - 48,
				w: n - e + 96,
				h: r - t + 96
			} : {
				x: 0,
				y: 0,
				w: 640,
				h: 480
			};
		}), Qn = t(() => {
			let e = /* @__PURE__ */ new Map();
			if (G.value) return ar.value ?? e;
			let t = z.value.boxes.map((e) => ({
				id: e.id,
				w: e.w,
				h: e.h,
				fixedX: e.x,
				fixedY: e.y
			}));
			return t.length === 0 ? e : Te(t, cr.value.map((e) => ({
				from: e.from,
				to: e.to
			})), { iterations: 1 }).routes;
		}), $n = t(() => {
			let e = new Map(K.value.map((e) => [e.id, e])), t = $.value, n = He.value, r = n !== void 0 && (O.edgeRouter ?? "") === "avoid" ? K.value.map((e) => ({
				x: e.x,
				y: e.y,
				w: e.w,
				h: e.h
			})) : void 0, i = /* @__PURE__ */ new Map();
			for (let [e, t] of Qn.value) i.set(e, t);
			let a = /* @__PURE__ */ new Map();
			for (let e of cr.value) {
				let t = `${e.from}->${e.to}`;
				a.set(t, (a.get(t) ?? 0) + 1);
			}
			return cr.value.flatMap((a) => {
				let o = e.get(a.from), s = e.get(a.to);
				if (!o || !s) return [];
				let c = a.fromSide, l = a.toSide;
				t && t.edgeId === a.id && (t.which === "from" ? c = t.side : l = t.side);
				let u = n({
					from: {
						x: o.x,
						y: o.y,
						w: o.w,
						h: o.h
					},
					to: {
						x: s.x,
						y: s.y,
						w: s.w,
						h: s.h
					},
					fromSide: c,
					toSide: l,
					waypoints: i.get(`${a.from}->${a.to}`),
					obstacles: r,
					parallelIndex: 0,
					parallelCount: 1
				});
				`${a.from}${a.to}`;
				let d = u.x2 - u.x1, f = u.y2 - u.y1, p = Math.hypot(d, f) || 1;
				return [{
					id: a.id,
					fromId: o.id,
					toId: s.id,
					fromTitle: o.title,
					toTitle: s.title,
					label: a.label,
					fromSide: a.fromSide,
					toSide: a.toSide,
					d: u.d,
					x1: u.x1,
					y1: u.y1,
					x2: u.x2,
					y2: u.y2,
					labelX: u.labelX,
					labelY: u.labelY,
					delX: u.labelX + -f / p * 18,
					delY: u.labelY + 7 + d / p * 18
				}];
			});
		}), W = g(null);
		function er(e) {
			jr.value || X.value || $.value || (W.value = e);
		}
		let tr = t(() => {
			if (!W.value) return /* @__PURE__ */ new Set();
			let e = /* @__PURE__ */ new Set([W.value]);
			for (let t of z.value.relations) t.from === W.value && e.add(t.to), t.to === W.value && e.add(t.from);
			return e;
		}), nr = (e) => W.value !== null && !tr.value.has(e), rr = (e) => W.value !== null && (e.fromId === W.value || e.toId === W.value), ir = (e) => W.value !== null && !rr(e), G = g(null), ar = g(null), K = t(() => {
			let e = V.value.size > 0 ? Ke.value : null, t = (e ? e.boxes : z.value.boxes).map((e) => {
				let t = G.value?.get(e.id);
				return t ? {
					...e,
					x: t.x,
					y: t.y
				} : e;
			});
			if (!e || e.groupNodes.length === 0) return t;
			let n = e.groupNodes.map((e) => {
				let t = G.value?.get(e.id);
				return {
					id: e.id,
					title: e.title,
					docPath: "",
					depth: 0,
					blocks: [],
					x: t?.x ?? e.x,
					y: t?.y ?? e.y,
					w: e.w,
					h: e.h,
					attrs: {
						type: "group",
						memberCount: e.memberCount
					}
				};
			});
			return [...t, ...n];
		}), or = t(() => Ke.value.groupNodes.map((e) => {
			let t = G.value?.get(e.id);
			return t ? {
				...e,
				x: t.x,
				y: t.y
			} : e;
		})), sr = t(() => {
			let e = /* @__PURE__ */ new Map();
			for (let t of Ke.value.groupNodes) e.set(t.id, t);
			return e;
		}), cr = t(() => V.value.size > 0 ? Ke.value.relations : z.value.relations), lr = t(() => V.value.size > 0 ? ur(Ke.value.expandedGroups, Ke.value.boxes) : fr.value);
		function ur(e, t) {
			let n = new Map(t.map((e) => [e.id, e]));
			return e.map((e) => {
				if (e.explicit) return e;
				let t = e.members.map((e) => n.get(e)).filter((e) => e !== void 0);
				return t.length === 0 ? e : {
					...e,
					...Me(t)
				};
			});
		}
		function dr(e, t) {
			let n = new Map(G.value ?? []);
			n.set(e, t), G.value = n;
		}
		let fr = t(() => {
			let e = ki.value, t = Ai.value;
			return z.value.groups.map((n) => {
				if (t && t.moved && t.name === n.name) return {
					...n,
					x: t.baseRegion.x,
					y: t.baseRegion.y,
					w: t.curW,
					h: t.curH
				};
				if (n.explicit) return e && e.moved && e.name === n.name ? {
					...n,
					x: e.baseRegion.x + e.dx,
					y: e.baseRegion.y + e.dy
				} : n;
				let r = n.members.map((e) => K.value.find((t) => t.id === e)).filter((e) => !!e);
				return {
					...n,
					...Me(r)
				};
			});
		}), pr = (e) => W.value !== null && !e.members.some((e) => tr.value.has(e));
		function mr() {
			if (G.value) {
				G.value = null, ar.value = null;
				return;
			}
			let e = K.value, t = Te(e.map((e) => ({
				id: e.id,
				w: e.w,
				h: e.h
			})), cr.value.map((e) => ({
				from: e.from,
				to: e.to
			})), {}), n = new Map(t.positions);
			if (q.value) for (let e of M.value.keys()) {
				let t = z.value.boxes.find((t) => t.docPath === e);
				t && n.delete(t.id);
			}
			G.value = n, ar.value = t.routes;
		}
		let hr = (e) => e.blocks.slice(0, 6), gr = (e) => Math.max(0, e.blocks.length - 6), _r = (e) => (hr(e).length + +(gr(e) > 0)) * An + 12, vr = (e) => e.y + e.h + 6 + _r(e) > Zn.value.y + Zn.value.h, q = g(!1), J = g("select"), Y = /* @__PURE__ */ new Set(), yr = g({
			title: "",
			message: "",
			action: null
		}), br = g(!1);
		function xr(e, t, n) {
			yr.value = {
				title: e,
				message: t,
				action: n
			}, br.value = !0;
		}
		function Sr() {
			br.value = !1, yr.value.action?.();
		}
		function Cr(e) {
			return M.value.get(e) ?? O.files[e];
		}
		function wr(e, t) {
			let n = new Map(M.value);
			t === O.files[e] ? (n.delete(e), Y.delete(e), P.delete(e)) : (n.has(e) || P.set(e, O.files[e] ?? null), n.set(e, t), e in O.files || Y.add(e)), M.value = n;
		}
		let Tr = g(!1);
		async function Er() {
			if (!Ee.value || Tr.value) return;
			Tr.value = !0;
			let e = [], t = [], n = 0;
			for (let [t, r] of M.value) {
				let i = await he(t, r, (P.has(t) ? P.get(t) : O.files[t] ?? null) ?? void 0);
				i.ok ? i.merged && n++ : (e.push(t), we("保存", t, i));
			}
			for (let e of N.value) {
				let n = await _e(e, O.files[e]);
				n.ok || (t.push(e), we("删除", e, n));
			}
			if (e.length > 0 || t.length > 0) {
				let n = new Map(M.value);
				for (let t of n.keys()) e.includes(t) || (n.delete(t), Y.delete(t), P.delete(t));
				M.value = n, N.value = new Set([...N.value].filter((e) => t.includes(e)));
			} else M.value = /* @__PURE__ */ new Map(), N.value = /* @__PURE__ */ new Set(), Y.clear(), P.clear(), F.value = [], I.value = [], j(n > 0 ? `图修改已保存（${n} 个文件已自动合并磁盘上的外部修改）` : "图修改已保存", "success", 3e3);
			Tr.value = !1;
		}
		function Dr() {
			if (!Ee.value) return;
			let e = new Set([...M.value.keys()].map((e) => z.value.boxes.find((t) => t.docPath === e)?.id).filter((e) => !!e));
			if (M.value = /* @__PURE__ */ new Map(), N.value = /* @__PURE__ */ new Set(), G.value) {
				let t = new Map(G.value);
				e.forEach((e) => t.delete(e)), G.value = t.size > 0 ? t : null;
			}
			Z.value = null, J.value = "select", Y.clear(), P.clear(), F.value = [], I.value = [], q.value = !1;
		}
		function Or() {
			if (q.value) {
				if (Ee.value) return;
				Z.value = null, J.value = "select", F.value = [], I.value = [], q.value = !1;
			} else q.value = !0;
		}
		let kr = g(null);
		function Ar(e, t) {
			let n = Yn.value?.toCanvasCoords?.(e, t);
			return n ? {
				x: n.x,
				y: n.y,
				scale: n.zoom
			} : {
				x: 0,
				y: 0,
				scale: 1
			};
		}
		let jr = g(null);
		function Mr(e, t, n) {
			let r = Yn.value?.getViewportRect?.();
			if (!r) return {
				px: 0,
				py: 0
			};
			let i = (e) => Math.max(0, Math.min(1, (40 - e) / 40)), a = 0, o = 0;
			return e < r.left + 40 ? a = -14 * i(e - r.left) : e > r.right - 40 && (a = 14 * i(r.right - e)), t < r.top + 40 ? o = -14 * i(t - r.top) : t > r.bottom - 40 && (o = 14 * i(r.bottom - t)), (a || o) && Yn.value?.panBy?.(-a, -o), {
				px: a / n,
				py: o / n
			};
		}
		let Nr = g([]), Pr = {
			x: [
				"start",
				"center",
				"end"
			],
			y: [
				"start",
				"center",
				"end"
			]
		}, Fr = {
			x: ["end"],
			y: ["end"]
		};
		function Ir(e, t) {
			return t === "x" ? {
				start: e.x,
				center: e.x + e.w / 2,
				end: e.x + e.w
			} : {
				start: e.y,
				center: e.y + e.h / 2,
				end: e.y + e.h
			};
		}
		function Lr(e, t, n, r) {
			let i = Math.min(Math.max(8 / n, 4), 12), a = {};
			for (let n of ["x", "y"]) {
				let o = Ir(e, n), s = r[n].map((e) => o[e]), c = null;
				for (let e of t) for (let t of Object.values(Ir(e, n))) for (let e of s) {
					let n = t - e;
					Math.abs(n) <= i && (c === null || Math.abs(n) < Math.abs(c)) && (c = n);
				}
				c !== null && (a[n === "x" ? "dx" : "dy"] = c);
			}
			return a;
		}
		function Rr(e, t, n) {
			let r = [], i = /* @__PURE__ */ new Set();
			for (let a of ["x", "y"]) {
				let o = Ir(e, a), s = n[a].map((e) => o[e]);
				for (let n of t) for (let t of Object.values(Ir(n, a))) {
					if (!s.some((e) => Math.abs(e - t) < .5)) continue;
					let o = `${a}${t}`, c = a === "x" ? Math.min(e.y, n.y) : Math.min(e.x, n.x), l = a === "x" ? Math.max(e.y + e.h, n.y + n.h) : Math.max(e.x + e.w, n.x + n.w), u = i.has(o) ? r.find((e) => e.axis === a && e.pos === t) : void 0;
					u ? (u.start = Math.min(u.start, c), u.end = Math.max(u.end, l)) : (i.add(o), r.push({
						axis: a,
						pos: t,
						start: c,
						end: l
					}));
				}
			}
			return r;
		}
		function zr(e, t, n, r) {
			let i = K.value.find((t) => t.id === e);
			if (!i) return {
				x: Math.round(t),
				y: Math.round(n),
				guides: []
			};
			let a = K.value.filter((t) => t.id !== e), o = Lr({
				x: t,
				y: n,
				w: i.w,
				h: i.h
			}, a, r, Pr), s = Math.round(t + (o.dx ?? 0)), c = Math.round(n + (o.dy ?? 0));
			return {
				x: s,
				y: c,
				guides: o.dx !== void 0 || o.dy !== void 0 ? Rr({
					x: s,
					y: c,
					w: i.w,
					h: i.h
				}, a, Pr) : []
			};
		}
		let Br = !1;
		function Vr(e, t) {
			if (q.value && e.button === 0 && !e.target.closest("button")) {
				if (J.value === "link") {
					Qr(e, t);
					return;
				}
				J.value !== "node" && (jr.value = {
					id: t.id,
					path: t.docPath,
					startClientX: e.clientX,
					startClientY: e.clientY,
					lastClientX: e.clientX,
					lastClientY: e.clientY,
					scale: Ar(e.clientX, e.clientY).scale,
					baseX: t.x,
					baseY: t.y,
					moved: !1,
					raf: 0,
					panX: 0,
					panY: 0,
					before: L()
				}, k({
					move: Hr,
					end: Gr
				}), W.value = null);
			}
		}
		function Hr(e) {
			let t = jr.value;
			t && (t.lastClientX = e.clientX, t.lastClientY = e.clientY, t.raf ||= requestAnimationFrame(Ur));
		}
		function Ur() {
			let e = jr.value;
			if (!e) return;
			e.raf = 0;
			let t = Mr(e.lastClientX, e.lastClientY, e.scale);
			e.panX += t.px, e.panY += t.py;
			let n = (e.lastClientX - e.startClientX) / e.scale + e.panX, r = (e.lastClientY - e.startClientY) / e.scale + e.panY;
			if (!e.moved && Math.hypot(n, r) < 3) return;
			e.moved = !0;
			let i = zr(e.id, e.baseX + n, e.baseY + r, e.scale);
			dr(e.id, {
				x: i.x,
				y: i.y
			}), Nr.value = i.guides;
		}
		function Wr() {
			let e = jr.value;
			if (jr.value = null, Nr.value = [], !e || (e.raf && cancelAnimationFrame(e.raf), !e.moved)) return;
			Br = !0;
			let t = G.value?.get(e.id);
			if (!t) return;
			let n = Cr(e.path);
			n !== void 0 && (wr(e.path, Ce(n, t)), R(e.before));
		}
		function Gr() {
			A({
				move: Hr,
				end: Gr
			}), Wr();
		}
		let { isTouch: Kr } = oe(), qr = g(null);
		function Jr(e) {
			if (Br) {
				Br = !1;
				return;
			}
			if (!q.value) {
				if (Kr.value && e.blocks.length > 0 && qr.value !== e.id) {
					qr.value = e.id;
					return;
				}
				qr.value = null, Gi(e.docPath);
			}
		}
		let X = g(null), Yr = g(null), Xr = null;
		function Zr(e, t) {
			let n = t.x - (e.x + e.w / 2), r = t.y - (e.y + e.h / 2);
			return Math.abs(n) * e.h > Math.abs(r) * e.w ? n >= 0 ? "right" : "left" : r >= 0 ? "bottom" : "top";
		}
		function Qr(e, t, n) {
			if (!q.value || e.button !== 0) return;
			e.preventDefault();
			let r = Ar(e.clientX, e.clientY);
			X.value = {
				fromId: t.id,
				fromSide: n ?? Zr(t, r),
				targetId: null,
				x: r.x,
				y: r.y,
				lastClientX: e.clientX,
				lastClientY: e.clientY,
				raf: 0,
				before: L()
			}, k({
				move: ei,
				end: ii,
				cancel: ri
			}), W.value = null;
		}
		function $r(e, t, n) {
			Qr(e, t, n);
		}
		function ei(e) {
			let t = X.value;
			t && (t.lastClientX = e.clientX, t.lastClientY = e.clientY, t.raf ||= requestAnimationFrame(ti));
		}
		function ti() {
			let e = X.value;
			if (!e) return;
			e.raf = 0, Mr(e.lastClientX, e.lastClientY, 1);
			let t = Ar(e.lastClientX, e.lastClientY), n = K.value.find((e) => t.x >= e.x && t.x <= e.x + e.w && t.y >= e.y && t.y <= e.y + e.h);
			X.value = {
				...e,
				x: t.x,
				y: t.y,
				targetId: n?.id ?? null
			};
		}
		function ni() {
			A({
				move: ei,
				end: ii,
				cancel: ri
			});
		}
		function ri() {
			ni();
			let e = X.value;
			e?.raf && cancelAnimationFrame(e.raf), X.value = null;
		}
		function ii(e) {
			ni();
			let t = X.value;
			if (t?.raf && cancelAnimationFrame(t.raf), X.value = null, !t) return;
			let n = Ar(e.clientX, e.clientY), r = K.value.find((e) => n.x >= e.x && n.x <= e.x + e.w && n.y >= e.y && n.y <= e.y + e.h);
			!r || r.id === t.fromId || z.value.relations.some((e) => e.from === t.fromId && e.to === r.id) || ai(t.fromId, r.id, t.fromSide, Zr(r, n), t.before);
		}
		function ai(e, t, n, r, i) {
			let a = z.value.boxes.find((t) => t.id === e);
			if (!a) return;
			let o = Cr(a.docPath);
			if (o === void 0) return;
			let s = ke({
				ref: t,
				fromSide: n,
				toSide: r
			});
			wr(a.docPath, xe(o, [...ve(o), s])), i && R(i), Yr.value = `${e}->${t}`, Xr && clearTimeout(Xr), Xr = setTimeout(() => {
				Yr.value = null, Xr = null;
			}, 700);
		}
		let oi = t(() => {
			let e = X.value;
			if (!e) return null;
			let t = K.value.find((t) => t.id === e.fromId);
			if (!t) return null;
			let n = e.targetId ? K.value.find((t) => t.id === e.targetId) : void 0, r = He.value;
			if (n && n.id !== e.fromId) {
				let i = Zr(n, {
					x: e.x,
					y: e.y
				});
				return r({
					from: {
						x: t.x,
						y: t.y,
						w: t.w,
						h: t.h
					},
					to: {
						x: n.x,
						y: n.y,
						w: n.w,
						h: n.h
					},
					fromSide: e.fromSide,
					toSide: i
				}).d;
			}
			return r({
				from: {
					x: t.x,
					y: t.y,
					w: t.w,
					h: t.h
				},
				to: {
					x: e.x,
					y: e.y,
					w: 0,
					h: 0
				},
				fromSide: e.fromSide
			}).d;
		});
		function si(e) {
			let t = X.value;
			return !t || t.targetId !== e.id ? null : e.id === t.fromId || z.value.relations.some((n) => n.from === t.fromId && n.to === e.id) ? "invalid" : "valid";
		}
		m(() => {
			window.addEventListener("keydown", Vi), window.addEventListener("popstate", Wi);
			let e = Re();
			e > 0 && (q.value = !0, u(() => j(`已恢复 ${e} 项上次未保存的图修改，可「💾 保存」或「↩ 放弃更改」`, "info", 6e3))), U.value || u(() => requestAnimationFrame(() => Yn.value?.fit?.())), tt();
		}), p(() => {
			Xr && clearTimeout(Xr), window.removeEventListener("keydown", Vi), window.removeEventListener("popstate", Wi), rt(), me();
			for (let e of [
				jr,
				X,
				$,
				ki,
				Ai
			]) {
				let t = e.value?.raf;
				t && cancelAnimationFrame(t);
			}
		});
		let ci = null, li = {
			end: (e) => di(e),
			cancel: () => {
				ci = null, A(li);
			}
		};
		function ui(e) {
			!q.value || J.value !== "node" || e.button === 0 && e.target === kr.value && (ci = {
				clientX: e.clientX,
				clientY: e.clientY
			}, k(li));
		}
		function di(e) {
			A(li);
			let t = ci;
			if (ci = null, !t || Math.hypot(e.clientX - t.clientX, e.clientY - t.clientY) >= 3) return;
			let n = Ar(e.clientX, e.clientY);
			fi(n.x, n.y);
		}
		function fi(e, t) {
			let n = Math.round(e - 220 / 2), r = Math.round(t - 96 / 2), i = L(), a = /* @__PURE__ */ new Set([...Object.keys(O.files), ...M.value.keys()]), o = 1;
			for (; a.has(`untitled-${o}.md`);) o++;
			let s = `untitled-${o}.md`, c = `未命名文档 ${o}`;
			wr(s, `---\ntitle: "${c}"\nx: ${n}\ny: ${r}\n---\n\n# ${c}\n`), R(i);
		}
		let Z = g(null), Q = t(() => $n.value.find((e) => e.id === Z.value) ?? null);
		function pi(e) {
			q.value && (Z.value = e.id);
		}
		let mi = g(null), hi = g(null);
		function gi(e) {
			q.value && (Z.value = e.id, mi.value = {
				edgeId: e.id,
				value: e.label ?? ""
			}, u(() => {
				hi.value?.focus(), hi.value?.select();
			}));
		}
		function _i() {
			let e = mi.value;
			if (mi.value = null, !e) return;
			let t = $n.value.find((t) => t.id === e.edgeId);
			if (!t) return;
			let n = e.value.trim();
			if (n === (t.label ?? "")) return;
			let r = z.value.boxes.find((e) => e.id === t.fromId);
			if (!r) return;
			let i = Cr(r.docPath);
			if (i === void 0) return;
			let a = L(), o = ve(i).map((e) => {
				let r = Oe(e);
				return Di(r.ref) === t.toId ? ke({
					ref: r.ref,
					label: n || void 0,
					fromSide: r.fromSide,
					toSide: r.toSide
				}) : e;
			});
			wr(r.docPath, xe(i, o)), R(a);
		}
		function vi() {
			mi.value = null;
		}
		let $ = g(null);
		function yi(e, t, n) {
			let r = t - (e.x + e.w / 2), i = n - (e.y + e.h / 2);
			return Math.abs(r) / (e.w / 2) >= Math.abs(i) / (e.h / 2) ? r >= 0 ? "right" : "left" : i >= 0 ? "bottom" : "top";
		}
		function bi(e, t, n) {
			if (e.button !== 0) return;
			e.preventDefault(), e.stopPropagation();
			let r = Ue(K.value.find((e) => e.id === t.fromId), K.value.find((e) => e.id === t.toId));
			$.value = {
				edgeId: t.id,
				which: n,
				side: (n === "from" ? t.fromSide : t.toSide) ?? (n === "from" ? r.fromSide : r.toSide),
				lastClientX: e.clientX,
				lastClientY: e.clientY,
				raf: 0,
				before: L()
			}, k({
				move: xi,
				end: Ti,
				cancel: wi
			}), W.value = null;
		}
		function xi(e) {
			let t = $.value;
			t && (t.lastClientX = e.clientX, t.lastClientY = e.clientY, t.raf ||= requestAnimationFrame(Si));
		}
		function Si() {
			let e = $.value;
			if (!e) return;
			e.raf = 0;
			let t = $n.value.find((t) => t.id === e.edgeId);
			if (!t) return;
			let n = K.value.find((n) => n.id === (e.which === "from" ? t.fromId : t.toId));
			if (!n) return;
			let r = Ar(e.lastClientX, e.lastClientY), i = yi(n, r.x, r.y);
			i !== e.side && ($.value = {
				...e,
				side: i
			});
		}
		function Ci() {
			A({
				move: xi,
				end: Ti,
				cancel: wi
			});
		}
		function wi() {
			Ci();
			let e = $.value;
			e?.raf && cancelAnimationFrame(e.raf), $.value = null;
		}
		function Ti() {
			Ci();
			let e = $.value;
			if (e?.raf && cancelAnimationFrame(e.raf), $.value = null, !e) return;
			let t = $n.value.find((t) => t.id === e.edgeId);
			if (!t) return;
			let n = e.which === "from" ? e.side : t.fromSide, r = e.which === "to" ? e.side : t.toSide;
			n === t.fromSide && r === t.toSide || (Ei(t, n, r), R(e.before));
		}
		function Ei(e, t, n) {
			let r = z.value.boxes.find((t) => t.id === e.fromId);
			if (!r) return;
			let i = Cr(r.docPath);
			if (i === void 0) return;
			let a = ve(i).map((r) => {
				let i = Oe(r);
				return Di(i.ref) === e.toId ? ke({
					ref: i.ref,
					label: i.label,
					fromSide: t,
					toSide: n
				}) : r;
			});
			wr(r.docPath, xe(i, a));
		}
		function Di(e) {
			let t = e.trim(), n = t.endsWith(".md") ? t : t + ".md", r = z.value.boxes;
			return (r.find((e) => e.id === t) ?? r.find((e) => e.docPath === t) ?? r.find((e) => e.docPath === n))?.id;
		}
		function Oi() {
			let e = Q.value;
			if (!e) return;
			let t = z.value.boxes.find((t) => t.id === e.fromId);
			if (!t) return;
			let n = Cr(t.docPath);
			if (n === void 0) return;
			let r = L(), i = ve(n).filter((t) => Di(Oe(t).ref) !== e.toId);
			wr(t.docPath, xe(n, i)), R(r), Z.value = null;
		}
		let ki = g(null), Ai = g(null);
		function ji(e, t) {
			if (!q.value || e.button !== 0) return;
			e.preventDefault();
			let n = fr.value.find((e) => e.name === t.name);
			if (!n) return;
			let r = /* @__PURE__ */ new Map();
			for (let e of t.members) {
				let t = K.value.find((t) => t.id === e);
				t && r.set(e, {
					x: t.x,
					y: t.y
				});
			}
			ki.value = {
				name: t.name,
				startClientX: e.clientX,
				startClientY: e.clientY,
				lastClientX: e.clientX,
				lastClientY: e.clientY,
				scale: Ar(e.clientX, e.clientY).scale,
				basePositions: r,
				baseRegion: {
					x: n.x,
					y: n.y,
					w: n.w,
					h: n.h
				},
				dx: 0,
				dy: 0,
				moved: !1,
				raf: 0,
				panX: 0,
				panY: 0,
				before: L()
			}, k({
				move: Mi,
				end: Pi
			}), W.value = null;
		}
		function Mi(e) {
			let t = ki.value;
			t && (t.lastClientX = e.clientX, t.lastClientY = e.clientY, t.raf ||= requestAnimationFrame(Ni));
		}
		function Ni() {
			let e = ki.value;
			if (!e) return;
			e.raf = 0;
			let t = Mr(e.lastClientX, e.lastClientY, e.scale);
			e.panX += t.px, e.panY += t.py;
			let n = Math.round((e.lastClientX - e.startClientX) / e.scale + e.panX), r = Math.round((e.lastClientY - e.startClientY) / e.scale + e.panY);
			if (!e.moved && Math.hypot(n, r) < 3) return;
			let i = [...K.value.filter((t) => !e.basePositions.has(t.id)), ...fr.value.filter((t) => t.name !== e.name)], a = Lr({
				x: e.baseRegion.x + n,
				y: e.baseRegion.y + r,
				w: e.baseRegion.w,
				h: e.baseRegion.h
			}, i, e.scale, Pr), o = n + (a.dx ?? 0), s = r + (a.dy ?? 0);
			ki.value = {
				...e,
				dx: o,
				dy: s,
				moved: !0
			};
			for (let [t, n] of e.basePositions) dr(t, {
				x: n.x + o,
				y: n.y + s
			});
			Nr.value = a.dx !== void 0 || a.dy !== void 0 ? Rr({
				x: e.baseRegion.x + o,
				y: e.baseRegion.y + s,
				w: e.baseRegion.w,
				h: e.baseRegion.h
			}, i, Pr) : [];
		}
		function Pi() {
			A({
				move: Mi,
				end: Pi
			});
			let e = ki.value;
			if (ki.value = null, Nr.value = [], !e || (e.raf && cancelAnimationFrame(e.raf), !e.moved)) return;
			let t = z.value.groups.find((t) => t.name === e.name);
			if (t) {
				for (let n of t.members) {
					let t = z.value.boxes.find((e) => e.id === n), r = e.basePositions.get(n);
					if (!t || !r) continue;
					let i = Cr(t.docPath);
					i !== void 0 && wr(t.docPath, Ce(i, {
						x: r.x + e.dx,
						y: r.y + e.dy
					}));
				}
				if (t.explicit) {
					let n = Cr(t.holder);
					n !== void 0 && wr(t.holder, Se(n, je({
						name: t.name,
						x: e.baseRegion.x + e.dx,
						y: e.baseRegion.y + e.dy,
						w: e.baseRegion.w,
						h: e.baseRegion.h
					})));
				}
				R(e.before);
			}
		}
		function Fi(e, t) {
			if (!q.value || e.button !== 0) return;
			e.preventDefault();
			let n = fr.value.find((e) => e.name === t.name);
			if (!n) return;
			let r = t.members.map((e) => K.value.find((t) => t.id === e)).filter((e) => !!e), i = Math.max(...r.map((e) => e.x + e.w)), a = Math.max(...r.map((e) => e.y + e.h));
			Ai.value = {
				name: t.name,
				startClientX: e.clientX,
				startClientY: e.clientY,
				lastClientX: e.clientX,
				lastClientY: e.clientY,
				scale: Ar(e.clientX, e.clientY).scale,
				baseRegion: {
					x: n.x,
					y: n.y,
					w: n.w,
					h: n.h
				},
				memberIds: new Set(t.members),
				minW: Math.max(48, i - n.x + 24),
				minH: Math.max(48, a - n.y + 24),
				curW: n.w,
				curH: n.h,
				moved: !1,
				raf: 0,
				before: L()
			}, k({
				move: Ii,
				end: Ri
			}), W.value = null;
		}
		function Ii(e) {
			let t = Ai.value;
			t && (t.lastClientX = e.clientX, t.lastClientY = e.clientY, t.raf ||= requestAnimationFrame(Li));
		}
		function Li() {
			let e = Ai.value;
			if (!e) return;
			e.raf = 0;
			let t = (e.lastClientX - e.startClientX) / e.scale, n = (e.lastClientY - e.startClientY) / e.scale;
			if (!e.moved && Math.hypot(t, n) < 3) return;
			let r = Math.round(e.baseRegion.w + t), i = Math.round(e.baseRegion.h + n), a = [...K.value.filter((t) => !e.memberIds.has(t.id)), ...fr.value.filter((t) => t.name !== e.name)], o = Lr({
				x: e.baseRegion.x,
				y: e.baseRegion.y,
				w: r,
				h: i
			}, a, e.scale, Fr), s = Math.max(e.minW, Math.round(r + (o.dx ?? 0))), c = Math.max(e.minH, Math.round(i + (o.dy ?? 0)));
			Ai.value = {
				...e,
				curW: s,
				curH: c,
				moved: !0
			}, Nr.value = o.dx !== void 0 || o.dy !== void 0 ? Rr({
				x: e.baseRegion.x,
				y: e.baseRegion.y,
				w: s,
				h: c
			}, a, Fr) : [];
		}
		function Ri() {
			A({
				move: Ii,
				end: Ri
			});
			let e = Ai.value;
			if (Ai.value = null, Nr.value = [], !e || (e.raf && cancelAnimationFrame(e.raf), !e.moved)) return;
			let t = z.value.groups.find((t) => t.name === e.name);
			if (!t) return;
			let n = Cr(t.holder);
			n !== void 0 && (wr(t.holder, Se(n, je({
				name: t.name,
				x: e.baseRegion.x,
				y: e.baseRegion.y,
				w: e.curW,
				h: e.curH
			}))), R(e.before));
		}
		function zi(e) {
			if (N.value.has(e.docPath)) {
				let t = L();
				N.value = new Set([...N.value].filter((t) => t !== e.docPath)), R(t);
				return;
			}
			if (Y.has(e.docPath) && !(e.docPath in O.files)) {
				Bi(e);
				return;
			}
			xr("删除文档", `「${e.title}」（${e.docPath}）将在「💾 保存」后从磁盘删除，保存前可撤销。确定标记删除？`, () => Bi(e));
		}
		function Bi(e) {
			let t = L();
			if (M.value.has(e.docPath)) {
				let t = new Map(M.value);
				t.delete(e.docPath), M.value = t;
			}
			Y.delete(e.docPath), P.delete(e.docPath), e.docPath in O.files && (N.value = /* @__PURE__ */ new Set([...N.value, e.docPath])), R(t);
		}
		function Vi(e) {
			if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
				e.preventDefault(), pt.value ? Ln() : In();
				return;
			}
			if (!(U.value || !q.value) && !mi.value) {
				if ((e.ctrlKey || e.metaKey) && (e.key === "z" || e.key === "Z" || e.key === "y")) {
					e.preventDefault(), e.key === "y" || e.shiftKey ? Pe() : Ne();
					return;
				}
				if (e.key === "Escape") {
					if (Z.value) {
						Z.value = null;
						return;
					}
					J.value !== "select" && (J.value = "select");
					return;
				}
				Z.value && (e.key === "Delete" || e.key === "Backspace") && (e.preventDefault(), Oi());
			}
		}
		let Hi = t(() => U.value ? z.value.boxes.find((e) => e.docPath === U.value)?.title ?? U.value : "");
		function Ui() {
			let e = U.value ? `#${encodeURIComponent(U.value)}` : "#";
			if (window.location.hash === e) {
				history.replaceState(null, "", e);
				return;
			}
			history.pushState(null, "", e);
		}
		function Wi() {
			let e = window.location.hash, t = null;
			if (e.length > 1) try {
				t = decodeURIComponent(e.slice(1));
			} catch {
				t = null;
			}
			Xi.value = !1, U.value = t && O.files[t] ? t : null;
		}
		function Gi(e) {
			if (O.files[e]) {
				if (U.value === e) {
					Xi.value = !1;
					return;
				}
				Xi.value = !1, U.value = e, le("navigate", e), Ui();
			}
		}
		function Ki(e) {
			document.querySelector(`.pd-doc-view [data-heading-id$="-${e}"]`)?.scrollIntoView({
				behavior: "smooth",
				block: "start"
			});
		}
		function qi(e, t) {
			if (U.value === e) {
				Ki(t);
				return;
			}
			Gi(e), u(() => {
				setTimeout(() => Ki(t), 80), setTimeout(() => Ki(t), 320);
			});
		}
		function Ji() {
			if (Xi.value && $i.value) {
				xr("丢弃未保存的修改？", "正文有未保存的修改，返回图画布将丢弃这些修改。", Yi);
				return;
			}
			Yi();
		}
		function Yi() {
			Xi.value = !1, U.value = null, Ui(), u(() => requestAnimationFrame(() => Yn.value?.fit?.()));
		}
		b(() => O.files, (e) => {
			if (Tr.value = !1, U.value && !e[U.value] && Yi(), M.value.size) {
				let t = new Map(M.value);
				for (let [n, r] of t) e[n] === r ? (t.delete(n), Y.delete(n), P.delete(n)) : e[n] === void 0 && !Y.has(n) && (t.delete(n), P.delete(n));
				M.value = t;
			}
			if (N.value.size) {
				let t = new Set([...N.value].filter((t) => e[t] !== void 0));
				t.size !== N.value.size && (N.value = t);
			}
			if (!G.value) return;
			let t = z.value.boxes, n = new Map(G.value);
			for (let [e, r] of n) {
				let i = t.find((t) => t.id === e);
				(!i || i.x === r.x && i.y === r.y) && n.delete(e);
			}
			G.value = n.size > 0 ? n : null;
		});
		let Xi = g(!1), Zi = g(""), Qi = g(""), $i = t(() => U.value !== null && Zi.value !== (O.files[U.value] ?? ""));
		function ea() {
			U.value && (Zi.value = O.files[U.value] ?? "", Qi.value = Zi.value, Xi.value = !0);
		}
		function ta(e) {
			Gi(e), ea();
		}
		function na() {
			Xi.value = !1;
		}
		async function ra() {
			if (!U.value || !$i.value) return;
			let e = await he(U.value, Zi.value, Qi.value);
			if (!e.ok) {
				we("保存", U.value, e, Zi.value);
				return;
			}
			e.merged && typeof e.content == "string" ? (Zi.value = e.content, Qi.value = e.content, j("已保存，并自动合并了磁盘上的外部修改", "success", 4e3)) : Qi.value = Zi.value;
		}
		function ia(e) {
			(e.ctrlKey || e.metaKey) && e.key === "s" && (e.preventDefault(), ra());
		}
		function aa(e, t) {
			if (q.value) {
				if (J.value !== "select") return;
				let n = e.shiftKey ? 10 : 1, r = {
					ArrowLeft: [-n, 0],
					ArrowRight: [n, 0],
					ArrowUp: [0, -n],
					ArrowDown: [0, n]
				}[e.key];
				if (!r) return;
				e.preventDefault(), sa(t, r[0], r[1]);
				return;
			}
			(e.key === "Enter" || e.key === " ") && (e.preventDefault(), Gi(t.docPath));
		}
		let oa = null;
		function sa(e, t, n) {
			let r = Math.round(e.x + t), i = Math.round(e.y + n);
			if (r === e.x && i === e.y) return;
			let a = Date.now(), o = oa?.id === e.id && a - oa.time < 800 ? null : L();
			oa = {
				id: e.id,
				time: a
			}, dr(e.id, {
				x: r,
				y: i
			});
			let s = Cr(e.docPath);
			s !== void 0 && (wr(e.docPath, Ce(s, {
				x: r,
				y: i
			})), o && R(o));
		}
		function ca(e, t) {
			if (/^(https?:|mailto:)/.test(t)) return null;
			let [n, r] = t.split("#"), i = n.trim(), a = r?.trim(), o;
			if (a) try {
				o = Ie(decodeURIComponent(a));
			} catch {
				o = Ie(a);
			}
			if (!i) return o ? {
				path: e,
				anchor: o
			} : null;
			if (!i.endsWith(".md")) return null;
			let s = i.startsWith("/") ? i.split("/") : [...e.split("/").slice(0, -1), ...i.split("/")], c = [];
			for (let e of s) e === "" || e === "." || (e === ".." ? c.pop() : c.push(e));
			return {
				path: c.join("/"),
				anchor: o
			};
		}
		function la(e) {
			if (!U.value) return;
			let t = ca(U.value, e);
			t && (t.anchor ? qi(t.path, t.anchor) : Gi(t.path));
		}
		function ua(e) {
			if (!U.value) return;
			let t = O.files[U.value];
			if (t === void 0) return;
			let n = de(t, e.source, e.id, e.x, e.y);
			if (n === t) return;
			let r = U.value;
			he(r, n, t).then((e) => {
				e.ok || we("保存", r, e);
			});
		}
		if (typeof window < "u" && window.location.hash.length > 1) {
			let e = decodeURIComponent(window.location.hash.slice(1));
			O.files[e] && (U.value = e);
		}
		return (t, u) => (h(), i("div", gt, [a("header", _t, [
			u[19] ||= a("span", { class: "pd-graph-brand" }, "📚 ProDoc", -1),
			U.value ? (h(), i("span", vt, v(Hi.value), 1)) : r("", !0),
			a("div", yt, [
				U.value ? r("", !0) : (h(), i(e, { key: 0 }, [
					q.value ? (h(), i(e, { key: 1 }, [
						a("button", {
							class: "pd-back-btn",
							disabled: F.value.length === 0,
							title: "撤销（Ctrl+Z）",
							"aria-label": "撤销",
							onClick: Ne
						}, "↶", 8, bt),
						a("button", {
							class: "pd-back-btn",
							disabled: I.value.length === 0,
							title: "重做（Ctrl+Shift+Z / Ctrl+Y）",
							"aria-label": "重做",
							onClick: Pe
						}, "↷", 8, xt),
						a("button", {
							class: "pd-back-btn",
							disabled: !Ee.value || Tr.value,
							onClick: Er
						}, "💾 保存", 8, St),
						Ee.value ? (h(), i("button", {
							key: 0,
							class: "pd-back-btn",
							disabled: Tr.value,
							onClick: Dr
						}, "↩ 放弃更改", 8, Ct)) : (h(), i("button", {
							key: 1,
							class: "pd-back-btn pd-back-btn--active",
							onClick: Or
						}, "✓ 完成"))
					], 64)) : (h(), i("button", {
						key: 0,
						class: "pd-back-btn",
						onClick: Or
					}, "🛠 编辑图")),
					a("button", {
						class: "pd-back-btn",
						onClick: mr
					}, v(G.value ? "↩ 恢复坐标" : "🧭 分层重排"), 1),
					z.value.groups.length ? (h(), i("button", {
						key: 2,
						class: "pd-back-btn",
						title: V.value.size ? "展开全部分组" : "折叠全部分组",
						onClick: Je
					}, v(V.value.size ? "⊞ 展开分组" : "⊟ 折叠分组"), 9, wt)) : r("", !0)
				], 64)),
				a("button", {
					class: "pd-back-btn pd-search-btn",
					title: "搜索文档（Ctrl/Cmd+K）",
					onClick: In
				}, " 🔍 搜索 "),
				U.value ? (h(), i(e, { key: 1 }, [Xi.value ? (h(), i(e, { key: 1 }, [a("button", {
					class: "pd-back-btn",
					disabled: !$i.value,
					onClick: ra
				}, "💾 保存", 8, Tt), a("button", {
					class: "pd-back-btn",
					onClick: na
				}, "👁 预览")], 64)) : (h(), i("button", {
					key: 0,
					class: "pd-back-btn",
					onClick: ea
				}, "✏️ 编辑")), a("button", {
					class: "pd-back-btn",
					onClick: Ji
				}, "🗺 返回图")], 64)) : r("", !0),
				z.value.warnings.length ? (h(), n(y(ae), {
					key: 2,
					trigger: "click",
					position: "bottom",
					width: 360
				}, {
					default: x(() => [a("span", Et, [s(y(re), { value: z.value.warnings.length }, {
						default: x(() => [...u[18] ||= [a("button", {
							class: "pd-back-btn",
							type: "button",
							title: "解析告警明细"
						}, "⚠️", -1)]]),
						_: 1
					}, 8, ["value"])])]),
					content: x(() => [a("ul", Dt, [(h(!0), i(e, null, _(z.value.warnings, (e) => (h(), i("li", { key: e }, v(e), 1))), 128))])]),
					_: 1
				})) : r("", !0),
				s(y(T), {
					modelValue: y(pe),
					"onUpdate:modelValue": u[0] ||= (e) => l(pe) ? pe.value = e : null,
					size: "small"
				}, null, 8, ["modelValue"])
			])
		]), a("div", Ot, [
			U.value ? (h(), i("div", {
				key: 1,
				class: d(["pd-doc-view", { "pd-doc-view--editing": Xi.value }])
			}, [Xi.value ? (h(), n(y(ce), {
				key: U.value,
				value: Zi.value,
				class: "pd-doc-editor",
				onChange: u[11] ||= (e) => Zi.value = e,
				onKeydown: ia
			}, null, 8, ["value"])) : (h(), n(y(ue), {
				key: U.value,
				content: Xe.value[U.value],
				"show-toc": !0,
				"flow-editable": !0,
				onDocLink: la,
				onFlowNodeMove: ua
			}, null, 8, ["content"]))], 2)) : (h(), n(y(ie), {
				key: 0,
				ref_key: "canvasRef",
				ref: Yn,
				width: "100%",
				height: "100%",
				"show-grid": "",
				"grid-variant": "dots",
				"show-fit": "",
				infinite: "",
				"content-bounds": Zn.value,
				"min-zoom": .25,
				"max-zoom": 3
			}, {
				default: x(() => [a("div", {
					ref_key: "stageEl",
					ref: kr,
					class: d(["pd-graph-stage", {
						"pd-graph-stage--dragging": jr.value?.moved || X.value || $.value || ki.value?.moved || Ai.value?.moved,
						"pd-graph-stage--editing": q.value,
						"pd-graph-stage--tool-link": q.value && J.value === "link",
						"pd-graph-stage--tool-node": q.value && J.value === "node"
					}]),
					style: f({
						width: `${Xn.value.w}px`,
						height: `${Xn.value.h}px`
					}),
					onPointerdown: ui,
					onClick: u[10] ||= (e) => {
						Z.value = null, qr.value = null;
					}
				}, [
					(h(!0), i(e, null, _(lr.value, (e) => (h(), i("div", {
						key: "group-" + e.name,
						class: d(["pd-doc-group", { "pd-dim": pr(e) }]),
						style: f({
							left: `${e.x}px`,
							top: `${e.y}px`,
							width: `${e.w}px`,
							height: `${e.h}px`
						})
					}, [a("span", {
						class: "pd-doc-group__label",
						title: q.value ? `拖动移动整组「${e.name}」；点击折叠` : `点击折叠「${e.name}」`,
						role: "button",
						tabindex: "0",
						"data-nm-no-pan": "",
						onPointerdown: (t) => ji(t, e),
						onClick: C((t) => qe(e.name), ["stop"]),
						onKeydown: [S(C((t) => qe(e.name), ["stop", "prevent"]), ["enter"]), S(C((t) => qe(e.name), ["stop", "prevent"]), ["space"])]
					}, [o(v(e.name) + " ", 1), u[20] ||= a("span", {
						class: "pd-doc-group__collapse",
						"aria-hidden": "true"
					}, "⊟", -1)], 40, kt), q.value ? (h(), i("button", {
						key: 0,
						type: "button",
						class: "pd-doc-group__resize",
						"aria-label": `调整组「${e.name}」的区域尺寸`,
						title: "拖动调整区域尺寸",
						"data-nm-no-pan": "",
						onPointerdown: C((t) => Fi(t, e), ["stop"])
					}, null, 40, At)) : r("", !0)], 6))), 128)),
					lt.value.length || oi.value ? (h(), i("svg", {
						key: 0,
						class: "pd-relation-layer",
						width: Xn.value.w,
						height: Xn.value.h,
						"aria-label": "文档连线"
					}, [
						u[21] ||= a("defs", null, [a("marker", {
							id: "pd-relation-arrow",
							markerWidth: "8",
							markerHeight: "8",
							refX: "7",
							refY: "4",
							orient: "auto",
							markerUnits: "strokeWidth"
						}, [a("path", {
							d: "M 0 0 L 8 4 L 0 8 z",
							class: "pd-relation-arrow"
						})])], -1),
						(h(!0), i(e, null, _(lt.value, (e) => (h(), i("g", {
							key: e.id,
							class: d(["pd-relation", {
								"pd-dim": ir(e),
								"pd-hot": rr(e),
								"pd-selected": e.id === Z.value,
								"pd-relation--new": e.id === Yr.value
							}])
						}, [
							a("title", null, v(e.fromTitle) + " → " + v(e.toTitle) + v(e.label ? `（${e.label}）` : ""), 1),
							a("path", {
								class: "pd-relation-hit",
								d: e.d,
								fill: "none",
								onClick: C((t) => pi(e), ["stop"]),
								onDblclick: C((t) => gi(e), ["stop"])
							}, null, 40, Mt),
							a("path", {
								d: e.d,
								fill: "none",
								"marker-end": "url(#pd-relation-arrow)",
								"pointer-events": "none"
							}, null, 8, Nt),
							e.label ? (h(), i("text", {
								key: 0,
								x: e.labelX,
								y: e.labelY,
								"pointer-events": "none"
							}, v(e.label), 9, Pt)) : r("", !0)
						], 2))), 128)),
						oi.value ? (h(), i("path", {
							key: 0,
							class: "pd-relation-draft",
							d: oi.value,
							fill: "none"
						}, null, 8, Ft)) : r("", !0),
						(h(!0), i(e, null, _(Nr.value, (e, t) => (h(), i("line", {
							key: "guide" + t,
							class: "pd-guide",
							x1: e.axis === "x" ? e.pos : e.start,
							y1: e.axis === "x" ? e.start : e.pos,
							x2: e.axis === "x" ? e.pos : e.end,
							y2: e.axis === "x" ? e.end : e.pos
						}, null, 8, It))), 128))
					], 8, jt)) : r("", !0),
					(h(!0), i(e, null, _(or.value, (e) => (h(), i("button", {
						key: e.id,
						type: "button",
						class: "pd-group-node",
						style: f({
							left: `${e.x}px`,
							top: `${e.y}px`,
							width: `${e.w}px`,
							height: `${e.h}px`
						}),
						"aria-label": `展开分组 ${e.name}（${e.memberCount} 篇文档）`,
						title: `${e.name}（${e.memberCount} 篇）— 点击展开`,
						"data-nm-no-pan": "",
						onClick: C((t) => qe(e.name), ["stop"]),
						onDblclick: C((t) => Ye(e.id), ["stop"]),
						onKeydown: S(C((t) => qe(e.name), ["stop", "prevent"]), ["enter"])
					}, [
						a("span", Rt, v(e.name), 1),
						a("span", zt, v(e.memberCount) + " 篇", 1),
						u[22] ||= a("span", { class: "pd-group-node__hint" }, "⊞ 点击展开", -1)
					], 44, Lt))), 128)),
					(h(!0), i(e, null, _(st.value, (t) => (h(), i("div", {
						key: t.id,
						class: d(["pd-doc-box", [`pd-doc-box--d${Math.min(t.depth, 3)}`, {
							"pd-dim": nr(t.id),
							"pd-doc-box--link-target": si(t) === "valid",
							"pd-doc-box--link-invalid": si(t) === "invalid",
							"pd-doc-box--deleting": N.value.has(t.docPath),
							"pd-doc-box--outline": ft.value === "outline",
							"pd-doc-box--compact": ft.value === "compact",
							[`pd-doc-box--type-${B(t)}`]: !0
						}]]),
						style: f({
							left: `${t.x}px`,
							top: `${t.y}px`,
							width: `${t.w}px`,
							height: `${t.h}px`
						}),
						role: "link",
						tabindex: "0",
						"aria-label": `${t.title}（跳转到文档）`,
						"data-nm-no-pan": "",
						onPointerdown: (e) => Vr(e, t),
						onClick: (e) => Jr(t),
						onKeydown: (e) => aa(e, t),
						onMouseenter: (e) => er(t.id),
						onMouseleave: u[3] ||= (e) => er(null)
					}, [
						Ge(t) ? (h(), n(ee(Ge(t)), {
							key: 0,
							box: t,
							badge: We(t),
							"node-type": B(t)
						}, null, 8, [
							"box",
							"badge",
							"node-type"
						])) : (h(), i("div", Vt, [
							a("span", {
								class: "pd-doc-box__title",
								title: t.title
							}, v(t.title), 9, Ht),
							We(t) ? (h(), i("span", Ut, v(We(t)), 1)) : r("", !0),
							u[23] ||= a("span", {
								class: "pd-doc-box__icon",
								"aria-hidden": "true"
							}, "↗", -1)
						])),
						q.value ? r("", !0) : (h(), i("button", {
							key: 2,
							type: "button",
							class: "pd-doc-box__edit",
							"aria-label": `编辑 ${t.title}`,
							title: "编辑文档",
							onClick: C((e) => ta(t.docPath), ["stop"]),
							onKeydown: [S(C((e) => ta(t.docPath), ["stop"]), ["enter"]), S(C((e) => ta(t.docPath), ["stop"]), ["space"])]
						}, "✏️", 40, Wt)),
						q.value && c.deleteHandler ? (h(), i("button", {
							key: 3,
							type: "button",
							class: d(["pd-doc-box__delete", { "pd-doc-box__delete--armed": N.value.has(t.docPath) }]),
							"aria-label": N.value.has(t.docPath) ? `撤销删除 ${t.title}` : `删除 ${t.title}`,
							title: N.value.has(t.docPath) ? "撤销删除标记" : "标记删除（💾 保存后生效）",
							onClick: C((e) => zi(t), ["stop"]),
							onPointerdown: u[1] ||= C(() => {}, ["stop"])
						}, v(N.value.has(t.docPath) ? "↩" : "✕"), 43, Gt)) : r("", !0),
						q.value ? (h(), i(e, { key: 4 }, _([
							"top",
							"right",
							"bottom",
							"left"
						], (e) => a("button", {
							key: e,
							type: "button",
							class: d(["pd-doc-box__link-handle", `pd-doc-box__link-handle--${e}`]),
							"aria-label": `从 ${t.title} 的${{
								top: "上",
								right: "右",
								bottom: "下",
								left: "左"
							}[e]}边创建连线（拖到目标框）`,
							title: "拖到其他框创建连线",
							onPointerdown: C((n) => $r(n, t, e), ["stop"]),
							onClick: u[2] ||= C(() => {}, ["stop"])
						}, null, 42, Kt)), 64)) : r("", !0),
						t.blocks.length && !q.value ? (h(), i("div", {
							key: 5,
							class: d(["pd-doc-blocks-pop", {
								"pd-doc-blocks-pop--above": vr(t),
								"pd-doc-blocks-pop--force": qr.value === t.id
							}])
						}, [a("div", qt, [(h(!0), i(e, null, _(hr(t), (e) => (h(), i("button", {
							key: e.anchor,
							type: "button",
							class: "pd-doc-blocks-pop__item",
							title: e.title,
							"aria-label": `跳转到「${e.title}」分块`,
							onClick: C((n) => qi(t.docPath, e.anchor), ["stop"]),
							onKeydown: [S(C((n) => qi(t.docPath, e.anchor), ["stop"]), ["enter"]), S(C((n) => qi(t.docPath, e.anchor), ["stop"]), ["space"])]
						}, "▸ " + v(e.title), 41, Jt))), 128)), gr(t) > 0 ? (h(), i("button", {
							key: 0,
							type: "button",
							class: "pd-doc-blocks-pop__item pd-doc-blocks-pop__item--more",
							"aria-label": `查看全部 ${t.blocks.length} 个分块`,
							onClick: C((e) => Gi(t.docPath), ["stop"])
						}, "+" + v(gr(t)) + " 更多分块…", 9, Yt)) : r("", !0)])], 2)) : r("", !0)
					], 46, Bt))), 128)),
					q.value && Q.value && !mi.value ? (h(), i("button", {
						key: 1,
						type: "button",
						class: "pd-edge-delete",
						style: f({
							left: `${Q.value.delX}px`,
							top: `${Q.value.delY}px`
						}),
						"aria-label": `删除连线 ${Q.value.fromTitle} → ${Q.value.toTitle}`,
						title: "删除连线（Delete）",
						onClick: C(Oi, ["stop"])
					}, "✕", 12, Xt)) : r("", !0),
					q.value && Q.value && mi.value && mi.value.edgeId === Q.value.id ? ne((h(), i("input", {
						key: 2,
						ref_key: "labelInputEl",
						ref: hi,
						"onUpdate:modelValue": u[4] ||= (e) => mi.value.value = e,
						type: "text",
						class: "pd-edge-label-input",
						style: f({
							left: `${Q.value.labelX}px`,
							top: `${Q.value.labelY}px`
						}),
						"aria-label": `编辑连线标签 ${Q.value.fromTitle} → ${Q.value.toTitle}`,
						placeholder: "连线标签（留空移除）",
						"data-nm-no-pan": "",
						onKeydown: [S(C(_i, ["prevent"]), ["enter"]), S(C(vi, ["prevent"]), ["esc"])],
						onBlur: _i,
						onClick: u[5] ||= C(() => {}, ["stop"])
					}, null, 44, Zt)), [[te, mi.value.value]]) : r("", !0),
					q.value && Q.value ? (h(), i("svg", {
						key: 3,
						class: "pd-relation-layer pd-relation-layer--top",
						width: Xn.value.w,
						height: Xn.value.h,
						"aria-hidden": "true"
					}, [a("g", $t, [a("circle", {
						class: "pd-edge-handle",
						cx: Q.value.x1,
						cy: Q.value.y1,
						r: "6"
					}, null, 8, en), a("circle", {
						class: "pd-edge-handle-halo",
						cx: Q.value.x1,
						cy: Q.value.y1,
						r: "12",
						onPointerdown: u[6] ||= C((e) => bi(e, Q.value, "from"), ["stop"]),
						onClick: u[7] ||= C(() => {}, ["stop"])
					}, [...u[24] ||= [a("title", null, "拖动调整源框连接边", -1)]], 40, tn)]), a("g", nn, [a("circle", {
						class: "pd-edge-handle",
						cx: Q.value.x2,
						cy: Q.value.y2,
						r: "6"
					}, null, 8, rn), a("circle", {
						class: "pd-edge-handle-halo",
						cx: Q.value.x2,
						cy: Q.value.y2,
						r: "12",
						onPointerdown: u[8] ||= C((e) => bi(e, Q.value, "to"), ["stop"]),
						onClick: u[9] ||= C(() => {}, ["stop"])
					}, [...u[25] ||= [a("title", null, "拖动调整目标框连接边", -1)]], 40, an)])], 8, Qt)) : r("", !0)
				], 38)]),
				_: 1
			}, 8, ["content-bounds"])),
			!U.value && q.value ? (h(), i("div", on, [
				a("button", {
					type: "button",
					class: d(["pd-edit-toolbar__btn", { "pd-edit-toolbar__btn--active": J.value === "select" }]),
					title: "选择工具（Esc）",
					onClick: u[12] ||= (e) => J.value = "select"
				}, "🖱 选择", 2),
				a("button", {
					type: "button",
					class: d(["pd-edit-toolbar__btn", { "pd-edit-toolbar__btn--active": J.value === "link" }]),
					title: "连线工具：从任意框拖到目标框创建连线",
					onClick: u[13] ||= (e) => J.value = "link"
				}, "🔗 连线", 2),
				a("button", {
					type: "button",
					class: d(["pd-edit-toolbar__btn", { "pd-edit-toolbar__btn--active": J.value === "node" }]),
					title: "节点工具：点画布空白创建新文档框",
					onClick: u[14] ||= (e) => J.value = "node"
				}, "📄 节点", 2)
			])) : r("", !0),
			pt.value ? (h(), i("div", {
				key: 3,
				class: "pd-search-overlay",
				role: "dialog",
				"aria-modal": "true",
				"aria-label": "搜索文档",
				onClick: C(Ln, ["self"])
			}, [a("div", sn, [
				a("div", cn, [
					u[26] ||= a("span", {
						class: "pd-search-icon",
						"aria-hidden": "true"
					}, "🔍", -1),
					ne(a("input", {
						ref_key: "searchInputRef",
						ref: Bn,
						"onUpdate:modelValue": u[15] ||= (e) => jn.value = e,
						type: "search",
						class: "pd-search-input",
						placeholder: "搜索标题、分块、正文…（↑↓ 选择，Enter 打开，Esc 关闭）",
						"aria-label": "搜索关键词",
						autocomplete: "off",
						onKeydown: zn
					}, null, 544), [[te, jn.value]]),
					a("button", {
						type: "button",
						class: "pd-search-close",
						"aria-label": "关闭搜索",
						onClick: Ln
					}, "✕")
				]),
				a("div", ln, [jn.value.trim() ? Fn.value.length === 0 ? (h(), i("p", dn, " 没有匹配「" + v(jn.value) + "」的内容 ", 1)) : (h(), i("ul", fn, [(h(!0), i(e, null, _(Fn.value, (t, n) => (h(), i("li", {
					key: `${t.boxId}-${t.kind}-${n}`,
					class: d(["pd-search-item", { "pd-search-item--active": n === Mn.value }])
				}, [a("button", {
					type: "button",
					class: "pd-search-hit",
					onClick: (e) => Rn(t),
					onMouseenter: (e) => Mn.value = n
				}, [a("span", mn, [a("span", hn, v(t.title), 1), a("span", gn, v(t.kind === "title" ? "标题" : t.kind === "path" ? "路径" : t.kind === "heading" ? "分块" : "正文"), 1)]), a("span", _n, [(h(!0), i(e, null, _(Vn(t), (t, n) => (h(), i(e, { key: n }, [t.mark ? (h(), i("mark", vn, v(t.text), 1)) : (h(), i(e, { key: 1 }, [o(v(t.text), 1)], 64))], 64))), 128))])], 40, pn)], 2))), 128))])) : (h(), i("p", un, "输入关键词开始搜索"))]),
				a("div", yn, " 共 " + v(Fn.value.length) + " 条结果 · " + v(Pn.value.length) + " 篇文档 ", 1)
			])])) : r("", !0),
			s(y(w), {
				"model-value": H.value !== null,
				title: "保存冲突",
				size: "medium",
				"confirm-label": "保留我的版本（覆盖磁盘）",
				"cancel-label": "保留磁盘版本",
				"onUpdate:modelValue": u[16] ||= (e) => {
					e || Wn();
				},
				onConfirm: Gn,
				onCancel: Kn
			}, {
				default: x(() => [H.value ? (h(), i(e, { key: 0 }, [
					a("p", bn, " 「" + v(H.value.path) + "」" + v(H.value.error), 1),
					Jn.value ? (h(), i("div", xn, [a("section", Sn, [a("h4", null, "磁盘当前 · " + v(Jn.value.diskTotal) + " 行", 1), Jn.value.disk ? (h(), i("pre", wn, [(h(!0), i(e, null, _(Jn.value.disk, (e) => (h(), i("span", {
						key: "d" + e.no,
						class: d(["pd-conflict-line", { "pd-conflict-line--diff": e.diff }])
					}, v(String(e.no).padStart(4, " ")) + " │ " + v(e.text) + "\n", 3))), 128))])) : (h(), i("pre", Cn, "（磁盘上已不存在该文件）"))]), a("section", Tn, [a("h4", null, "我的版本 · " + v(Jn.value.mineTotal) + " 行", 1), a("pre", En, [(h(!0), i(e, null, _(Jn.value.mine, (e) => (h(), i("span", {
						key: "m" + e.no,
						class: d(["pd-conflict-line", { "pd-conflict-line--diff": e.diff }])
					}, v(String(e.no).padStart(4, " ")) + " │ " + v(e.text) + "\n", 3))), 128))])])])) : r("", !0),
					Jn.value ? (h(), i("p", Dn, " 差异约 " + v(Jn.value.changed) + " 行（已展示差异处 ±" + v(kn) + " 行）。 「保留我的版本」会以磁盘当前内容为基准重新提交（覆盖对方改动）； 「保留磁盘版本」丢弃本次修改。两侧的改动都需要时，请手工合并后重新编辑。 ", 1)) : r("", !0)
				], 64)) : r("", !0)]),
				_: 1
			}, 8, ["model-value"]),
			s(y(E), {
				ref_key: "toastRef",
				ref: fe
			}, null, 512),
			s(y(w), {
				modelValue: br.value,
				"onUpdate:modelValue": u[17] ||= (e) => br.value = e,
				title: yr.value.title,
				size: "small",
				"confirm-label": "确认",
				"cancel-label": "取消",
				onConfirm: Sr
			}, {
				default: x(() => [o(v(yr.value.message), 1)]),
				_: 1
			}, 8, ["modelValue", "title"])
		])]));
	}
});
//#endregion
export { se as DocFlowCanvas, jn as DocGraphViewer, O as DocViewer, le as MarkdownRenderer };

