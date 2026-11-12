//#region src/frame.ts
function e(e) {
	let t = [], n = "", r = null;
	for (let i of e) r ? (n += i, i === r && (r = null)) : i === "\"" || i === "'" ? (r = i, n += i) : i === "," ? (t.push(n), n = "") : n += i;
	return t.push(n), t;
}
function t(n) {
	let r = n.trim();
	if (r === "") return "";
	if (r.startsWith("[") && r.endsWith("]")) return e(r.slice(1, -1)).map((e) => t(e)).filter((e) => e !== "");
	if (r === "true") return !0;
	if (r === "false") return !1;
	if (/^-?\d+$/.test(r)) return parseInt(r, 10);
	if (/^-?\d*\.\d+$/.test(r)) return parseFloat(r);
	let i = r.match(/^(["'])([\s\S]*)\1$/);
	return i ? i[2] : r;
}
function n(e) {
	let n = e.indexOf("\n");
	if ((n === -1 ? e : e.slice(0, n)).trim() !== "---") return {
		params: {},
		body: e,
		hasFrame: !1
	};
	let r = (n === -1 ? "" : e.slice(n + 1)).split("\n"), i = -1;
	for (let e = 0; e < r.length; e++) if (r[e].trim() === "---") {
		i = e;
		break;
	}
	if (i === -1) return {
		params: {},
		body: e,
		hasFrame: !1
	};
	let a = {}, o = 0, s = r.slice(0, i);
	for (; o < s.length;) {
		let e = s[o];
		if (o += 1, e.trim() === "") continue;
		let n = e.match(/^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*([\s\S]*)$/);
		if (!n) continue;
		let r = n[1], i = n[2];
		if (i.trim() === "" && o < s.length && s[o].trimStart().startsWith("[") && (i = s[o], o += 1), i.trimStart().startsWith("[") && !/\]\s*$/.test(i)) {
			let e = [i];
			for (; o < s.length;) {
				let t = s[o];
				if (o += 1, e.push(t), /\]\s*$/.test(t)) break;
			}
			i = e.join("\n");
		}
		a[r] = t(i);
	}
	return {
		params: a,
		body: r.slice(i + 1).join("\n"),
		hasFrame: !0
	};
}
function r(e) {
	return (Array.isArray(e) ? e : typeof e == "string" ? e.split(/,\s+/) : []).filter((e) => typeof e == "string").map((e) => e.trim()).filter(Boolean);
}
function i(e) {
	return r(n(e).params.link);
}
function a(e) {
	return e === "" || e !== e.trim() || /[|,"]/.test(e) ? e.includes("\"") ? `'${e}'` : `"${e}"` : e;
}
function o(e, t, r) {
	let i = e.includes("\r\n") ? "\r\n" : "\n";
	if (!n(e).hasFrame) return r === null ? e : `---${i}${r}${i}---${i}${e}`;
	let a = e.split(/\r?\n/), o = a.findIndex((e, t) => t > 0 && e.trim() === "---");
	if (o === -1) return e;
	let s = RegExp(`^${t}\\s*:`), c = a.slice(1, o).findIndex((e) => s.test(e));
	if (c >= 0) {
		let e = c + 1, t = 1, n = e, i = a[e].replace(s, "");
		if (i.trim() === "" && e + 1 < o && a[e + 1].trimStart().startsWith("[") && (n = e + 1, i = a[n], t = 2), i.trimStart().startsWith("[") && !/\]\s*$/.test(i)) {
			let r = n + 1;
			for (; r < o && !/\]\s*$/.test(a[r]);) r++;
			r < o && (t = r - e + 1);
		}
		r === null ? a.splice(e, t) : a.splice(e, t, r);
	} else r !== null && a.splice(o, 0, r);
	return a.join(i);
}
function s(e, t) {
	return o(e, "link", t.length > 0 ? `link: [${t.map(a).join(", ")}]` : null);
}
function c(e, t) {
	return o(e, "group", t === null ? null : `group: ${a(t)}`);
}
function l(e, t) {
	let r = Object.entries(t).filter((e) => (e[0] === "x" || e[0] === "y") && typeof e[1] == "number" && Number.isFinite(e[1]));
	if (r.length === 0) return e;
	let i = e.includes("\r\n") ? "\r\n" : "\n";
	if (!n(e).hasFrame) return `---${i}${r.map(([e, t]) => `${e}: ${t}`).join(i)}${i}---${i}${e}`;
	let a = e.split(/\r?\n/), o = a.findIndex((e, t) => t > 0 && e.trim() === "---");
	if (o === -1) return e;
	let s = o;
	for (let [e, t] of r) {
		let n = RegExp(`^${e}\\s*:`), r = a.slice(1, s).findIndex((e) => n.test(e));
		r >= 0 ? a[r + 1] = `${e}: ${t}` : (a.splice(s, 0, `${e}: ${t}`), s++);
	}
	return a.join(i);
}
function u(e, t) {
	let n = 0, r = e.length, i = t.length;
	for (; n < r && n < i && e[n] === t[n];) n++;
	for (; r > n && i > n && e[r - 1] === t[i - 1];) r--, i--;
	let a = [];
	for (let e = 0; e < n; e++) a.push([e, e]);
	let o = r - n, s = i - n;
	if (o > 0 && s > 0 && o * s <= 4e6) {
		let r = s + 1, i = new Uint32Array((o + 1) * r);
		for (let a = o - 1; a >= 0; a--) for (let o = s - 1; o >= 0; o--) i[a * r + o] = e[n + a] === t[n + o] ? i[(a + 1) * r + o + 1] + 1 : Math.max(i[(a + 1) * r + o], i[a * r + o + 1]);
		let c = 0, l = 0;
		for (; c < o && l < s;) e[n + c] === t[n + l] ? (a.push([n + c, n + l]), c++, l++) : i[(c + 1) * r + l] >= i[c * r + l + 1] ? c++ : l++;
	}
	for (let t = 0; t < e.length - r; t++) a.push([r + t, i + t]);
	return a;
}
function d(e, t) {
	let n = [], r = 0, i = 0;
	for (let [a, o] of u(e, t)) (r !== a || i !== o) && n.push({
		baseStart: r,
		baseEnd: a,
		lines: t.slice(i, o)
	}), r = a + 1, i = o + 1;
	return (r !== e.length || i !== t.length) && n.push({
		baseStart: r,
		baseEnd: e.length,
		lines: t.slice(i)
	}), n;
}
function f(e, t) {
	let n = e.baseStart === e.baseEnd, r = t.baseStart === t.baseEnd;
	return n && r ? e.baseStart === t.baseStart : e.baseStart < t.baseEnd && t.baseStart < e.baseEnd;
}
function p(e, t) {
	if (e.length !== t.length) return !1;
	for (let n = 0; n < e.length; n++) if (e[n] !== t[n]) return !1;
	return !0;
}
function m(e, t, n) {
	let r = d(e, t), i = d(e, n), a = [], o = 0, s = 0, c = 0, l = 0, u = (t) => {
		let n = Math.min(t, e.length);
		for (let t = s; t < n; t++) a.push(e[t]);
	};
	for (; c < r.length || l < i.length;) {
		let e = c < r.length ? r[c] : null, t = l < i.length ? i[l] : null;
		if (e && e.baseStart < s) {
			c++;
			continue;
		}
		if (t && t.baseStart < s) {
			l++;
			continue;
		}
		if (e && !t) {
			u(e.baseStart), a.push(...e.lines), s = e.baseEnd, c++;
			continue;
		}
		if (t && !e) {
			u(t.baseStart), a.push(...t.lines), s = t.baseEnd, l++;
			continue;
		}
		if (!e || !t) break;
		if (!f(e, t)) {
			let n = e.baseStart <= t.baseStart, r = n ? e : t;
			u(r.baseStart), a.push(...r.lines), s = r.baseEnd, n ? c++ : l++;
			continue;
		}
		if (e.baseStart === t.baseStart && e.baseEnd === t.baseEnd && p(e.lines, t.lines)) {
			u(e.baseStart), a.push(...e.lines), s = e.baseEnd, c++, l++;
			continue;
		}
		let n = [...e.lines], d = [...t.lines], m = Math.max(e.baseEnd, t.baseEnd), h = Math.min(e.baseStart, t.baseStart);
		for (c++, l++;;) {
			let e = !1, t = c < r.length ? r[c] : null;
			t && t.baseStart < m && (n.push(...t.lines), m = Math.max(m, t.baseEnd), c++, e = !0);
			let a = l < i.length ? i[l] : null;
			if (a && a.baseStart < m && (d.push(...a.lines), m = Math.max(m, a.baseEnd), l++, e = !0), !e) break;
		}
		u(h), o++, a.push("<<<<<<< 磁盘当前（外部修改）"), a.push(...n), a.push("======="), a.push(...d), a.push(">>>>>>> 本次保存（编辑器内容）"), s = m;
	}
	return u(e.length), {
		lines: a,
		conflicts: o
	};
}
function h(e, t, n) {
	if (t === n) return {
		clean: !0,
		result: t,
		conflicts: 0
	};
	if (e === t) return {
		clean: !0,
		result: n,
		conflicts: 0
	};
	if (e === n) return {
		clean: !0,
		result: t,
		conflicts: 0
	};
	let r = t.includes("\r\n") ? "\r\n" : "\n", i = /\r?\n/, { lines: a, conflicts: o } = m(e.split(i), t.split(i), n.split(i));
	return {
		clean: o === 0,
		result: a.join(r),
		conflicts: o
	};
}
function g(e, t, n) {
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
	let v = /* @__PURE__ */ new Map();
	for (let t = 0; t < r; t++) v.set(e[t], h[o[t]]);
	return v;
}
function _(e, t, n) {
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
	return v(i);
}
function v(e) {
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
function y(e, t) {
	let n = 0;
	for (let r = 0; r + 1 < e.length; r++) n += _(e[r], e[r + 1], t);
	return n;
}
function b(e, t, n = {}, r) {
	let i = n.padding ?? 48, a = n.gapX ?? 64, o = n.gapY ?? 72, s = n.iterations ?? 4, c = n.avoidFixed ?? !0, l = n.wrapThreshold ?? Infinity;
	new Map(e.map((e) => [e.id, e]));
	let u = e.map((e) => e.id), d = g(u, t, r), f = u.filter((e) => !t.some((t) => t.to === e)), p = /* @__PURE__ */ new Map();
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
	let m = /* @__PURE__ */ new Map(), h = /* @__PURE__ */ new Map(), v = (e, t) => {
		let n = m.get(e);
		n ? n.push(t) : m.set(e, [t]);
	}, b = /* @__PURE__ */ new Set();
	for (let e of t) {
		let t = `${e.from}->${e.to}`;
		if (b.has(t)) continue;
		b.add(t);
		let n = d.get(e.from), r = d.get(e.to);
		if (n === void 0 || r === void 0 || r <= n) continue;
		if (r - n === 1) {
			v(e.from, e.to);
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
			}), v(i, n), a.push(n), i = n;
		}
		v(i, e.to), a.length > 0 ? h.set(t, a.map((e) => ({
			x: 0,
			y: 0
		}))) : h.delete(t);
	}
	let x = Math.max(0, ...[...p.values()].map((e) => e.layer + 1)), S = Array.from({ length: x }, () => []);
	for (let e of p.values()) e.layer >= S.length && (S.length = e.layer + 1), S[e.layer] || (S[e.layer] = []), S[e.layer].push(e);
	let C = /* @__PURE__ */ new Map();
	for (let [e, t] of m) for (let n of t) {
		let t = C.get(n);
		t ? t.push(e) : C.set(n, [e]);
	}
	{
		let e = /* @__PURE__ */ new Map();
		for (let [, t] of m) t.forEach((t, n) => {
			e.has(t) || e.set(t, n);
		});
		let t = /* @__PURE__ */ new Map(), n = 0;
		for (let r of S) {
			let i = r.map((n) => {
				let r = C.get(n.id) ?? [], i = 2 ** 53 - 1;
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
	let w = (e) => S[e].map((e) => e.id), T = (e, t, n) => {
		let r = e.map((e, r) => {
			let i = (t.get(e.id) ?? []).map((e) => n.get(e)).filter((e) => e !== void 0);
			return {
				node: e,
				idx: r,
				bary: i.length === 0 ? r : i.reduce((e, t) => e + t, 0) / i.length
			};
		});
		return r.sort((e, t) => e.bary === t.bary ? e.idx - t.idx : e.bary - t.bary), r.map((e) => e.node);
	}, E = (e) => {
		let t = /* @__PURE__ */ new Map();
		return e.forEach((e, n) => t.set(e.id, n)), t;
	}, D = (e) => {
		let t = 0;
		return e > 0 && (t += _(w(e - 1), w(e), m)), e + 1 < S.length && (t += _(w(e), w(e + 1), m)), t;
	}, O = (e) => {
		let t = /* @__PURE__ */ new Map();
		e > 0 && S[e - 1].forEach((e, n) => t.set(e.id, n)), S[e].forEach((e, n) => t.set(e.id, n)), e + 1 < S.length && S[e + 1].forEach((e, n) => t.set(e.id, n));
		let n = 0, r = (e, r) => {
			let i = t.get(e), a = t.get(r);
			i !== void 0 && a !== void 0 && (n += Math.abs(i - a));
		};
		if (e > 0) for (let [n, i] of m) for (let a of i) t.has(n) && S[e].some((e) => e.id === a) && r(n, a);
		if (e + 1 < S.length) for (let t of S[e + 1]) for (let e of C.get(t.id) ?? []) r(e, t.id);
		return n;
	}, k = () => S.map((e, t) => w(t)), ee = (e) => {
		for (let t = 0; t < S.length && t < e.length; t++) {
			let n = new Map(e[t].map((e, t) => [e, t]));
			S[t].sort((e, t) => (n.get(e.id) ?? 0) - (n.get(t.id) ?? 0));
		}
	}, A = k(), j = y(A, m);
	for (let e = 0; e < s && j > 0; e++) {
		for (let e = 1; e < S.length; e++) S[e] = T(S[e], C, E(S[e - 1]));
		for (let e = S.length - 2; e >= 0; e--) S[e] = T(S[e], m, E(S[e + 1]));
		for (let e = 0; e < S.length; e++) {
			let t = !0, n = S[e].length * 2 + 8;
			for (; t && n-- > 0;) {
				t = !1;
				for (let n = 0; n + 1 < S[e].length; n++) {
					let r = D(e), i = O(e);
					[S[e][n], S[e][n + 1]] = [S[e][n + 1], S[e][n]];
					let a = D(e), o = O(e);
					a < r || a === r && o < i ? t = !0 : [S[e][n], S[e][n + 1]] = [S[e][n + 1], S[e][n]];
				}
			}
		}
		let e = k(), t = y(e, m);
		if (t < j) j = t, A = e;
		else break;
	}
	ee(A);
	let M = (e) => e.dummy ? 0 : e.w, N = S.map((e) => e.filter((e) => !e.dummy)), P = /* @__PURE__ */ new Map(), F = (e) => {
		let t = p.get(e);
		return (P.get(e) ?? 0) + M(t) / 2;
	};
	for (let e of N) {
		let t = 0;
		for (let n of e) {
			if (n.fixedX !== void 0) {
				P.set(n.id, n.fixedX), t = Math.max(t, n.fixedX + n.w + a);
				continue;
			}
			P.set(n.id, t), t += n.w + a;
		}
	}
	let I = /* @__PURE__ */ new Map(), L = /* @__PURE__ */ new Map();
	for (let e of t) {
		if (e.from === e.to || !p.has(e.from) || !p.has(e.to)) continue;
		let t = I.get(e.from);
		t ? t.push(e.to) : I.set(e.from, [e.to]);
		let n = L.get(e.to);
		n ? n.push(e.from) : L.set(e.to, [e.from]);
	}
	let R = e.some((e) => e.fixedX !== void 0 || e.fixedY !== void 0), z = .15, B = [];
	for (let e of N) for (let t of e) {
		let e = (I.get(t.id) ?? []).filter((e) => P.has(e) && p.get(e)?.dummy === !1);
		e.length > 0 && B.push({
			node: t,
			kids: e
		});
	}
	let V = [];
	for (let [e, t] of I) for (let n of t) e !== n && P.has(e) && P.has(n) && V.push([e, n]);
	let te = () => {
		let e = 0;
		for (let { node: t, kids: n } of B) {
			let r = 0;
			for (let e of n) r += F(e);
			r /= n.length;
			let i = F(t.id) - r;
			e += 1 * i * i;
		}
		for (let [t, n] of V) {
			let r = F(t) - F(n);
			e += z * r * r;
		}
		return e;
	}, ne = () => {
		let e = /* @__PURE__ */ new Map(), t = (t, n) => e.set(t, (e.get(t) ?? 0) + n);
		for (let { node: e, kids: n } of B) {
			let r = 0;
			for (let e of n) r += F(e);
			r /= n.length;
			let i = 2 * (F(e.id) - r);
			t(e.id, i);
			for (let e of n) t(e, -i / n.length);
		}
		for (let [e, n] of V) {
			let r = 2 * z * (F(e) - F(n));
			t(e, r), t(n, -r);
		}
		return e;
	}, re = () => {
		for (let e of N) {
			let t = Infinity;
			for (let n = e.length - 1; n >= 0; n--) {
				let r = e[n];
				if (r.fixedX !== void 0) {
					t = r.fixedX - a;
					continue;
				}
				let i = P.get(r.id);
				if (Number.isFinite(t)) {
					let e = Math.min(i, t - r.w);
					P.set(r.id, e), t = e - a;
				} else t = i - a;
			}
			t = -Infinity;
			for (let n of e) {
				if (n.fixedX !== void 0) {
					t = n.fixedX + n.w + a;
					continue;
				}
				let e = Math.max(P.get(n.id), t);
				P.set(n.id, e), t = e + n.w + a;
			}
		}
	};
	{
		let e = .1, t = te(), n = 0;
		for (; n < 500; n++) {
			let n = ne(), r = !1, i = e, a = 0;
			for (let o = 0; o < 24 && !r; o++) {
				let o = new Map(P);
				a = 0;
				for (let e of N) for (let t of e) {
					if (t.fixedX !== void 0) continue;
					let e = n.get(t.id);
					if (!e) continue;
					let r = P.get(t.id), o = r - i * e;
					a = Math.max(a, Math.abs(o - r)), P.set(t.id, o);
				}
				re();
				let s = te();
				if (s < t - 1e-9) r = !0, t = s, e = Math.min(i * 1.5, 1);
				else {
					for (let [e, t] of o) P.set(e, t);
					i *= .5;
				}
			}
			if (!r || a < .001) break;
		}
	}
	{
		let e = /* @__PURE__ */ new Map();
		for (let { node: t, kids: n } of B) e.set(t.id, n);
		for (let t = N.length - 2; t >= 0; t--) {
			for (let n of N[t]) {
				if (n.fixedX !== void 0) continue;
				let t = e.get(n.id);
				if (!t || t.length === 0) continue;
				let r = t.reduce((e, t) => e + F(t), 0) / t.length;
				P.set(n.id, r - n.w / 2);
			}
			re();
		}
	}
	let H = [], U = [], W = 0;
	for (let e = 0; e < N.length; e++) {
		let t = N[e];
		U.push(H.length);
		let n = l >= 1 ? Math.floor(l) : Infinity;
		if (!Number.isFinite(n) || t.length <= n) {
			H.push(t);
			continue;
		}
		W++;
		let r = Math.ceil(t.length / n), i = Math.ceil(t.length / r), o = Math.min(...t.map((e) => P.get(e.id) ?? 0));
		for (let e = 0; e < t.length; e += i) {
			let n = t.slice(e, e + i), r = o;
			for (let e of n) {
				if (e.fixedX !== void 0) {
					r = Math.max(r, e.fixedX + e.w + a);
					continue;
				}
				P.set(e.id, r), r += e.w + a;
			}
			H.push(n);
		}
	}
	if (W > 0) {
		let e = (e) => {
			let t = Infinity;
			for (let n of e) t = Math.min(t, P.get(n.id) ?? 0);
			return t;
		}, t = (e) => {
			let t = -Infinity;
			for (let n of e) t = Math.max(t, (P.get(n.id) ?? 0) + n.w);
			return t;
		};
		for (let e of H) {
			let t = 0;
			for (let n of e) {
				if (n.fixedX !== void 0) {
					t = Math.max(t, n.fixedX + n.w + a);
					continue;
				}
				P.set(n.id, t), t += n.w + a;
			}
		}
		for (let n = 0; n < 3; n++) for (let n = 1; n < H.length; n++) {
			let r = H[n];
			if (r.length === 0) continue;
			let i = 0, a = 0;
			for (let e of r) for (let t of L.get(e.id) ?? []) P.has(t) && (i += (P.get(t) ?? 0) + M(p.get(t)) / 2, a++);
			if (a === 0) continue;
			let o = i / a - (e(r) + t(r)) / 2;
			for (let e of r) e.fixedX === void 0 && P.set(e.id, (P.get(e.id) ?? 0) + o);
		}
	}
	let G = [], K = [], q = 0;
	for (let e = 0; e < H.length; e++) {
		let t = H[e], n = t.length > 0 ? Math.max(...t.map((e) => e.h)) : 0;
		G.push(q), K.push(n), q += n + o;
	}
	let J = /* @__PURE__ */ new Map();
	for (let e = 0; e < H.length; e++) for (let t of H[e]) {
		if (t.fixedY !== void 0) {
			J.set(t.id, t.fixedY);
			continue;
		}
		J.set(t.id, G[e] + (K[e] - t.h) / 2);
	}
	let Y = 0;
	if (!R) {
		let e = Infinity, t = Infinity;
		for (let n of p.values()) n.dummy || (e = Math.min(e, P.get(n.id) ?? Infinity), t = Math.min(t, J.get(n.id) ?? Infinity));
		let n = Number.isFinite(e) ? i - e : 0, r = Number.isFinite(t) ? i - t : 0;
		Y = r;
		for (let e of p.values()) e.dummy || (P.set(e.id, (P.get(e.id) ?? 0) + n), J.set(e.id, (J.get(e.id) ?? 0) + r));
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
				let r = P.get(n.id), i = J.get(n.id);
				for (let e = 0; e < 4096 && t(r, i, n.w, n.h); e++) r += a;
				P.set(n.id, r), e.push({
					x: r,
					y: i,
					w: n.w,
					h: n.h
				});
			}
		}
	}
	let X = /* @__PURE__ */ new Map();
	for (let e of p.values()) e.dummy || X.set(e.id, {
		x: Math.round(P.get(e.id) ?? 0),
		y: Math.round(J.get(e.id) ?? 0)
	});
	for (let [e, t] of h) {
		let [n, r] = e.split("->"), i = X.get(n), a = X.get(r), s = p.get(n), c = p.get(r);
		if (!i || !a || !s || !c) continue;
		let l = i.x + s.w / 2, u = a.x + c.w / 2, f = d.get(n) ?? 0;
		t.forEach((e, n) => {
			let r = (n + 1) / (t.length + 1), i = U[f + n + 1], a = i !== void 0 && G[i] !== void 0 ? G[i] - o / 2 + Y : 0;
			t[n] = {
				x: Math.round((l * (1 - r) + u * r) * 100) / 100,
				y: Math.round(a * 100) / 100
			};
		});
	}
	return {
		positions: X,
		layers: d,
		routes: h,
		crossings: j,
		roots: f
	};
}
//#endregion
//#region src/graph.ts
var x = 220, S = 96, C = 6, w = 24, T = 34, E = /* @__PURE__ */ new Set([
	"id",
	"title",
	"x",
	"y",
	"w",
	"h",
	"link",
	"group"
]);
function D(e) {
	return typeof e == "number" && Number.isFinite(e) ? e : void 0;
}
var O = {
	t: "top",
	r: "right",
	b: "bottom",
	l: "left"
}, k = {
	top: "t",
	right: "r",
	bottom: "b",
	left: "l"
}, ee = /^([trbl_])>([trbl_])$/;
function A(e) {
	let t = e.split("|").map((e) => e.trim()), n = { ref: t[0] };
	for (let e of t.slice(1)) {
		let t = e.match(ee);
		t ? (t[1] !== "_" && (n.fromSide = O[t[1]]), t[2] !== "_" && (n.toSide = O[t[2]])) : e !== "" && (n.label = e);
	}
	return n;
}
function j(e) {
	let t = e.ref;
	if (e.label && (t += ` | ${e.label}`), e.fromSide || e.toSide) {
		let n = e.fromSide ? k[e.fromSide] : "_", r = e.toSide ? k[e.toSide] : "_";
		t += ` | ${n}>${r}`;
	}
	return t;
}
var M = /^(.*)\s*@\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*$/;
function N(e) {
	let t = e.match(M);
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
function P(e) {
	let { name: t, x: n, y: r, w: i, h: a } = e;
	return [
		n,
		r,
		i,
		a
	].every((e) => typeof e == "number" && Number.isFinite(e)) ? `${t} @ ${n}, ${r}, ${i}, ${a}` : t;
}
function F(e, t) {
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
function I(e) {
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
function L(e) {
	return /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(e.trim());
}
function R(e) {
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
function z(e) {
	return e.toLowerCase().replace(/[^\p{L}\p{N}\s-]/gu, "").replace(/[\s-]+/g, "-").replace(/^-+|-+$/g, "");
}
function B(e) {
	return e.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1").replace(/\[([^\]]*)\]\([^)]*\)/g, "$1").replace(/[`*_~]/g, "").replace(/<[^>]+>/g, "").trim();
}
function V(e) {
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
			let e = B(i[1]);
			e && t.push({
				anchor: z(e),
				title: e
			});
		}
	}
	return t.length >= 2 ? t : [];
}
function te(e, t, n) {
	return g(e.map((e) => e.id), t.map((e) => ({
		from: e.from,
		to: e.to
	})), n);
}
function ne(e, t, n, r) {
	if (e.filter((e) => {
		let t = r.get(e.id);
		return t.rawX === void 0 || t.rawY === void 0;
	}).length === 0) return;
	let i = b(e.map((e) => {
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
function re(e) {
	let t = Object.keys(e).sort(), i = [], a = /* @__PURE__ */ new Map(), o = /* @__PURE__ */ new Map(), s = /* @__PURE__ */ new Map();
	for (let r of t) {
		let { params: t, body: c } = n(e[r]), l = typeof t.id == "string" && t.id.trim() !== "" ? t.id.trim() : r.replace(/\.md$/, ""), u = typeof t.title == "string" && t.title.trim() !== "" && t.title.trim() || I(c) || l, d = V(c), f = D(t.w) ?? 220, p = D(t.h) ?? 96, m = {};
		for (let [e, n] of Object.entries(t)) E.has(e) || (m[e] = n);
		let h = {
			id: l,
			title: u,
			docPath: r,
			depth: 0,
			blocks: d,
			x: D(t.x) ?? 0,
			y: D(t.y) ?? 0,
			w: f,
			h: p,
			attrs: m
		};
		if (o.set(l, {
			rawX: D(t.x),
			rawY: D(t.y)
		}), t.group !== void 0) {
			let e = (Array.isArray(t.group) ? t.group : [t.group]).map((e) => typeof e == "string" ? e : typeof e == "number" ? String(e) : "").filter((e) => e.trim() !== "");
			if (e.length > 1 && i.push(`文档 "${l}" 声明了多个 group，仅取第一个 "${N(e[0]).name || e[0]}"`), e.length > 0) {
				let { name: t, geo: n } = N(e[0]);
				t && s.set(l, {
					name: t,
					geo: n
				});
			}
		}
		a.has(l) && i.push(`重复 id "${l}"：${a.get(l).docPath} 被 ${r} 覆盖`), a.set(l, h);
	}
	let c = [...a.values()], l = new Map(c.map((e) => [e.docPath, e])), u = [], d = /* @__PURE__ */ new Set();
	function f(e) {
		if (e === "") return;
		let t = /\.md$/i.test(e) ? e : e + ".md";
		return a.get(e) ?? l.get(e) ?? l.get(t) ?? a.get(t.replace(/\.md$/i, ""));
	}
	function p(e, t) {
		let n = f(R(e));
		if (n) return n;
		if (!t) return;
		let r = t.includes("/") ? t.slice(0, t.lastIndexOf("/") + 1) : "";
		return r === "" ? void 0 : f(r + R(e));
	}
	function m(e, t, n, r) {
		let a = p(e), o = a ? p(t, a.docPath) : p(t);
		if (!a || !o) {
			let n = a ? t : e;
			i.push("连线 " + r + " 引用了不存在的文档 \"" + n + "\"");
			return;
		}
		if (a.id === o.id) {
			i.push("文档 \"" + a.id + "\" 不能连线自身");
			return;
		}
		let s = a.id + "->" + o.id;
		d.has(s) || (d.add(s), u.push({
			id: s,
			type: "link",
			from: a.id,
			to: o.id,
			label: n.label,
			fromSide: n.fromSide,
			toSide: n.toSide
		}));
	}
	for (let t of c) {
		let { params: i } = n(e[t.docPath]);
		for (let e of r(i.link)) {
			let { ref: n, label: r, fromSide: i, toSide: a } = A(e);
			n && (L(n) || m(t.id, n, {
				label: r,
				fromSide: i,
				toSide: a
			}, t.id + ".link"));
		}
	}
	let h = te(c, u, i);
	for (let e of c) e.depth = h.get(e.id) ?? 0;
	ne(c, u, h, o);
	let g = /* @__PURE__ */ new Map();
	for (let e of c) {
		let t = s.get(e.id);
		if (!t) continue;
		let n = g.get(t.name);
		n || (n = { members: [] }, g.set(t.name, n)), n.members.push(e), t.geo && (n.geo ? (n.geo.x !== t.geo.x || n.geo.y !== t.geo.y || n.geo.w !== t.geo.w || n.geo.h !== t.geo.h) && i.push(`组 "${t.name}" 的显式几何被多个成员声明且不一致，取 ${n.holder} 的声明`) : (n.geo = t.geo, n.holder = e.docPath));
	}
	return {
		boxes: c,
		relations: u,
		groups: [...g.entries()].map(([e, t]) => ({
			name: e,
			members: t.members.map((e) => e.id),
			...F(t.members, t.geo),
			explicit: t.geo !== void 0,
			holder: t.holder ?? t.members[0].docPath
		})),
		warnings: [...new Set(i)]
	};
}
//#endregion
//#region src/edge-router.ts
function H(e, t) {
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
function U(e, t) {
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
function W(e, t) {
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
var G = (e) => Math.round(e * 100) / 100, K = (e) => {
	let t = U(e.from, e.to), n = e.waypoints ?? [], r = (t, n) => {
		let r = H(e.from, t), i = H(e.to, n), a = W(e, r);
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
	}, i = e.from.y + e.from.h <= e.to.y || e.to.y + e.to.h <= e.from.y, a = e.fromSide ?? (i ? "bottom" : t.fromSide), o = e.toSide ?? (i ? "top" : t.toSide), { p1: s, p2: c, start: l, end: u } = r(a, o), d = q(l, u, s, n).map((e) => ({
		x: e.x + (l.x - s.x),
		y: e.y + (l.y - s.y)
	}));
	d.length === 0 && n.length > 0 && ({p1: s, p2: c, start: l, end: u} = r(t.fromSide, t.toSide));
	let f = Math.hypot(u.x - l.x, u.y - l.y), p = Math.max(24, Math.min(f * .45, 96)), m, h;
	d.length === 0 ? (h = [l, u], m = `M ${G(l.x)} ${G(l.y)} C ${G(l.x + s.nx * p)} ${G(l.y + s.ny * p)}, ${G(u.x + c.nx * p)} ${G(u.y + c.ny * p)}, ${G(u.x)} ${G(u.y)}`) : (h = [
		l,
		...d,
		u
	], m = J(h, s, c));
	let g = Y(h);
	return {
		d: m,
		x1: G(l.x),
		y1: G(l.y),
		x2: G(u.x),
		y2: G(u.y),
		labelX: G(g.x),
		labelY: G(g.y) - 7,
		fromSide: d.length === 0 && n.length > 0 ? t.fromSide : a,
		toSide: d.length === 0 && n.length > 0 ? t.toSide : o,
		points: h
	};
};
function q(e, t, n, r) {
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
function J(e, t, n) {
	if (e.length === 0) return "";
	if (e.length === 1) return `M ${G(e[0].x)} ${G(e[0].y)}`;
	let r = `M ${G(e[0].x)} ${G(e[0].y)}`;
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
		r += ` C ${G(d.x)} ${G(d.y)}, ${G(f.x)} ${G(f.y)}, ${G(o.x)} ${G(o.y)}`;
	}
	return r;
}
function Y(e) {
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
var X = 28, Z = (e) => {
	let t = U(e.from, e.to), n = e.fromSide ?? t.fromSide, r = e.toSide ?? t.toSide, i = H(e.from, n), a = H(e.to, r), o = W(e, i), s = {
		x: i.x + o.x,
		y: i.y + o.y
	}, c = {
		x: a.x + o.x,
		y: a.y + o.y
	}, l = {
		x: s.x + i.nx * X,
		y: s.y + i.ny * X
	}, u = {
		x: c.x + a.nx * X,
		y: c.y + a.ny * X
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
	let m = ie(f), h = Y(m);
	return {
		d: ae(m, 10),
		x1: G(s.x),
		y1: G(s.y),
		x2: G(c.x),
		y2: G(c.y),
		labelX: G(h.x),
		labelY: G(h.y) - 7,
		fromSide: n,
		toSide: r,
		points: m
	};
};
function ie(e) {
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
function ae(e, t) {
	if (e.length === 0) return "";
	if (e.length === 1) return `M ${G(e[0].x)} ${G(e[0].y)}`;
	let n = `M ${G(e[0].x)} ${G(e[0].y)}`;
	for (let r = 1; r < e.length; r++) {
		let i = e[r - 1], a = e[r];
		if (r === e.length - 1) {
			n += ` L ${G(a.x)} ${G(a.y)}`;
			continue;
		}
		let o = e[r + 1], s = Math.hypot(a.x - i.x, a.y - i.y), c = Math.hypot(o.x - a.x, o.y - a.y), l = Math.min(t, s / 2, c / 2);
		if (l < 1) {
			n += ` L ${G(a.x)} ${G(a.y)}`;
			continue;
		}
		let u = {
			x: a.x - (a.x - i.x) / s * l,
			y: a.y - (a.y - i.y) / s * l
		}, d = {
			x: a.x + (o.x - a.x) / c * l,
			y: a.y + (o.y - a.y) / c * l
		};
		n += ` L ${G(u.x)} ${G(u.y)} Q ${G(a.x)} ${G(a.y)}, ${G(d.x)} ${G(d.y)}`;
	}
	return n;
}
function oe(e, t, n, r) {
	let i = n.x - r, a = n.x + n.w + r, o = n.y - r, s = n.y + n.h + r, c = Math.min(e.x, t.x), l = Math.max(e.x, t.x), u = Math.min(e.y, t.y), d = Math.max(e.y, t.y);
	return u === d ? u > o && u < s && l > i && c < a : c === l ? c > i && c < a && d > o && u < s : !1;
}
var se = (e) => {
	let t = (e.obstacles ?? []).filter((t) => !(t.x === e.from.x && t.y === e.from.y && t.w === e.from.w && t.h === e.from.h) && !(t.x === e.to.x && t.y === e.to.y && t.w === e.to.w && t.h === e.to.h));
	if (t.length === 0) return Z(e);
	let n = U(e.from, e.to), r = e.fromSide ?? n.fromSide, i = e.toSide ?? n.toSide, a = H(e.from, r), o = H(e.to, i), s = {
		x: a.x,
		y: a.y
	}, c = {
		x: o.x,
		y: o.y
	}, l = /* @__PURE__ */ new Set([s.x, c.x]), u = /* @__PURE__ */ new Set([s.y, c.y]);
	for (let e of t) l.add(e.x - 18), l.add(e.x + e.w + 18), u.add(e.y - 18), u.add(e.y + e.h + 18);
	let d = [...l].sort((e, t) => e - t), f = [...u].sort((e, t) => e - t), p = new Map(d.map((e, t) => [e, t])), m = new Map(f.map((e, t) => [e, t])), h = (e, t) => t * d.length + e, g = d.length * f.length;
	if (g > 4e4) return Z(e);
	let _ = (e, n) => !t.some((t) => oe(e, n, t, 6)), v = p.get(s.x), y = m.get(s.y), b = p.get(c.x), x = m.get(c.y), S = new Float64Array(g).fill(Infinity), C = new Float64Array(g).fill(Infinity), w = new Int32Array(g).fill(-1), T = new Uint8Array(g), E = h(v, y), D = h(b, x), O = (e, t) => Math.abs(d[e] - c.x) + Math.abs(f[t] - c.y);
	S[E] = 0, C[E] = O(v, y);
	let k = [E], ee = (e, t) => {
		let n = [];
		return e > 0 && n.push([e - 1, t]), e + 1 < d.length && n.push([e + 1, t]), t > 0 && n.push([e, t - 1]), t + 1 < f.length && n.push([e, t + 1]), n;
	}, A = !1;
	for (; k.length > 0;) {
		let e = 0;
		for (let t = 1; t < k.length; t++) C[k[t]] < C[k[e]] && (e = t);
		let t = k.splice(e, 1)[0];
		if (t === D) {
			A = !0;
			break;
		}
		if (T[t]) continue;
		T[t] = 1;
		let n = t % d.length, r = (t - n) / d.length, i = {
			x: d[n],
			y: f[r]
		};
		for (let [e, a] of ee(n, r)) {
			let n = h(e, a);
			if (T[n]) continue;
			let r = {
				x: d[e],
				y: f[a]
			};
			if (!_(i, r)) continue;
			let o = Math.abs(r.x - i.x) + Math.abs(r.y - i.y), s = S[t] + o;
			s >= S[n] || (S[n] = s, C[n] = s + O(e, a), w[n] = t, k.push(n));
		}
	}
	if (!A) return Z(e);
	let j = [];
	for (let e = D; e !== -1; e = w[e]) {
		let t = e % d.length, n = (e - t) / d.length;
		if (j.push({
			x: d[t],
			y: f[n]
		}), e === E) break;
	}
	j.reverse();
	let M = ie(j), N = Y(M);
	return {
		d: ae(M, 10),
		x1: G(s.x),
		y1: G(s.y),
		x2: G(c.x),
		y2: G(c.y),
		labelX: G(N.x),
		labelY: G(N.y) - 7,
		fromSide: r,
		toSide: i,
		points: M
	};
}, ce = {
	bezier: K,
	orthogonal: Z,
	avoid: se
}, le = "bezier";
function ue(e) {
	return e ? ce[e] ?? K : K;
}
//#endregion
//#region src/plugin.ts
var de = "doc", fe = /^[a-z][a-z0-9_-]*$/;
function pe(e) {
	return e;
}
function me(e) {
	let t = [], n = /* @__PURE__ */ new Set();
	for (let r of e) {
		if (!r || typeof r.name != "string" || r.name.trim() === "") {
			t.push("插件缺少 name，已忽略");
			continue;
		}
		n.has(r.name) && t.push(`插件 "${r.name}" 重复注册，后注册者生效`), n.add(r.name);
		for (let e of r.nodeTypes ?? []) if (fe.test(e.name) || t.push(`插件 "${r.name}" 的节点类型名 "${e.name}" 不合法（需小写字母开头，仅含小写字母/数字/-/_）`), e.defaultSize) {
			let { w: n, h: i } = e.defaultSize;
			(!Number.isFinite(n) || !Number.isFinite(i) || n <= 0 || i <= 0) && t.push(`插件 "${r.name}" 的节点类型 "${e.name}" 默认尺寸非法，已忽略`);
		}
	}
	return t;
}
function he(e) {
	let t = e ?? [], n = me(t), r = /* @__PURE__ */ new Map(), i, a;
	for (let e of t) if (!(!e || typeof e.name != "string")) {
		for (let t of e.nodeTypes ?? []) {
			if (!t || typeof t.name != "string" || !fe.test(t.name)) continue;
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
function ge(e) {
	let t = e?.type;
	if (typeof t != "string") return "doc";
	let n = t.trim().toLowerCase();
	return fe.test(n) ? n : "doc";
}
function _e(e, t, n) {
	return e.map((e) => {
		let r = ge(e.attrs), i = t.get(r), a = i?.defaultSize, o = e.w !== n.w, s = e.h !== n.h;
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
function ve(e, t) {
	if (e !== "doc") return t.get(e)?.badge ?? e;
}
//#endregion
//#region src/collapse.ts
var ye = "\0group:";
function be(e) {
	return `${ye}${e}`;
}
function xe(e) {
	return e.startsWith(ye);
}
function Se(e, t) {
	let n = /* @__PURE__ */ new Map(), r = [], i = [];
	for (let a of e.groups) {
		if (!t.has(a.name) || a.members.length === 0) {
			i.push(a);
			continue;
		}
		let e = be(a.name);
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
function Ce(e) {
	return {
		id: e.id,
		w: e.w,
		h: e.h,
		fixedX: e.x,
		fixedY: e.y
	};
}
//#endregion
//#region src/search.ts
var Q = {
	title: 1e3,
	path: 400,
	heading: 300,
	body: 20
}, we = 2;
function Te(e) {
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
function $(e, t, n = 8) {
	if (t === "") return [];
	let r = [], i = 0;
	for (; r.length < n;) {
		let n = e.indexOf(t, i);
		if (n === -1) break;
		r.push(n), i = n + t.length;
	}
	return r;
}
function Ee(e, t, n, r) {
	let i = $(t, n);
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
function De(e, t, n = {}) {
	let r = t.trim().toLowerCase();
	if (r === "") return [];
	let i = n.limit ?? 20, a = n.context ?? 40, o = n.includeBody ?? !0, s = [];
	for (let t of e) {
		if (t.lowerTitle.includes(r)) {
			let { snippet: e, ranges: n } = Ee(t.title, t.lowerTitle, r, a), i = $(t.lowerTitle, r).length;
			s.push({
				docPath: t.docPath,
				boxId: t.id,
				title: t.title,
				kind: "title",
				snippet: e,
				ranges: n,
				score: Q.title + i * 10
			});
		}
		t.lowerPath.includes(r) && !t.lowerTitle.includes(r) && s.push({
			docPath: t.docPath,
			boxId: t.id,
			title: t.title,
			kind: "path",
			snippet: t.docPath,
			ranges: $(t.lowerPath, r).map((e) => [e, e + r.length]),
			score: Q.path
		});
		for (let e of t.lowerBlocks) e.lower.includes(r) && s.push({
			docPath: t.docPath,
			boxId: t.id,
			title: t.title,
			kind: "heading",
			snippet: e.title,
			ranges: $(e.lower, r).map((e) => [e, e + r.length]),
			anchor: e.anchor,
			score: Q.heading + $(e.lower, r).length * 5
		});
		if (o) {
			let e = $(t.lowerBody, r, we);
			for (let n of e) {
				let i = Math.max(0, n - a), o = Math.min(t.body.length, n + r.length + a), c = i > 0 ? "…" : "", l = c + t.body.slice(i, o) + (o < t.body.length ? "…" : ""), u = c.length;
				s.push({
					docPath: t.docPath,
					boxId: t.id,
					title: t.title,
					kind: "body",
					snippet: l,
					ranges: [[n - i + u, n - i + r.length + u]],
					score: Q.body + e.length
				});
			}
		}
	}
	return s.sort((e, t) => t.score === e.score ? e.docPath === t.docPath ? e.snippet < t.snippet ? -1 : +(e.snippet > t.snippet) : e.docPath < t.docPath ? -1 : 1 : t.score - e.score), s.slice(0, i);
}
function Oe(e) {
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
export { P as A, n as B, H as C, w as D, T as E, z as F, c as H, b as I, g as L, F as M, N, C as O, A as P, h as R, ue as S, x as T, s as U, i as V, l as W, U as _, Se as a, ce as b, xe as c, ve as d, pe as f, le as g, me as h, ye as i, j, re as k, de as l, ge as m, Oe as n, Ce as o, he as p, De as r, be as s, Te as t, _e as u, se as v, S as w, Z as x, K as y, r as z };

