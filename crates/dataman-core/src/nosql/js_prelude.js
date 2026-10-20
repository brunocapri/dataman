var __B = (function () {
  "use strict";
  var native = {
    op: __n_op, cursor: __n_cursor, print: __n_print, pretty: __n_pretty,
    oid: __n_oid, uuid: __n_uuid, date: __n_date, sleep: __n_sleep,
  };

  function hidden(o, k, v) {
    Object.defineProperty(o, k, { value: v, enumerable: false });
  }
  function method(ctor, name, f) {
    Object.defineProperty(ctor.prototype, name, { value: f, enumerable: false, writable: true, configurable: true });
  }

  function ObjectId(hex) {
    if (!new.target) return new ObjectId(hex);
    if (hex instanceof ObjectId) hex = hex.$hex;
    if (hex === undefined || hex === null) hex = native.oid();
    hex = String(hex);
    if (!/^[0-9a-fA-F]{24}$/.test(hex)) throw new TypeError('ObjectId("' + hex + '"): not 24 hex digits');
    hidden(this, "$hex", hex.toLowerCase());
  }
  method(ObjectId, "toString", function () { return this.$hex; });
  method(ObjectId, "toHexString", function () { return this.$hex; });
  method(ObjectId, "equals", function (o) { return (o instanceof ObjectId && o.$hex === this.$hex) || String(o) === this.$hex; });
  method(ObjectId, "getTimestamp", function () { return new Date(parseInt(this.$hex.slice(0, 8), 16) * 1000); });
  method(ObjectId, "toJSON", function () { return { $oid: this.$hex }; });
  method(ObjectId, "__shell", function () { return 'ObjectId("' + this.$hex + '")'; });
  ObjectId.isValid = function (h) { return /^[0-9a-fA-F]{24}$/.test(String(h)); };
  ObjectId.createFromTime = function (s) { return new ObjectId(Math.floor(s).toString(16).padStart(8, "0") + "0000000000000000"); };
  ObjectId.createFromHexString = function (h) { return new ObjectId(h); };

  function ISODate(s) {
    if (s === undefined) return new Date();
    if (s instanceof Date) return new Date(s.getTime());
    if (typeof s === "number") return new Date(s);
    return new Date(native.date(String(s)));
  }

  function NumberLong(v) {
    if (!new.target) return new NumberLong(v);
    var s;
    if (v instanceof NumberLong) s = v.$long;
    else if (v === undefined) s = "0";
    else if (typeof v === "number") s = String(Math.trunc(v));
    else s = String(v).trim();
    if (!/^-?\d+$/.test(s)) throw new TypeError("NumberLong(" + JSON.stringify(String(v)) + "): not an integer");
    hidden(this, "$long", BigInt(s).toString());
  }
  method(NumberLong, "valueOf", function () { return Number(this.$long); });
  method(NumberLong, "toNumber", function () { return Number(this.$long); });
  method(NumberLong, "toString", function (radix) { return BigInt(this.$long).toString(radix); });
  method(NumberLong, "toJSON", function () { return { $numberLong: this.$long }; });
  method(NumberLong, "equals", function (o) { return String(o instanceof NumberLong ? o.$long : o) === this.$long; });
  method(NumberLong, "__shell", function () { return 'NumberLong("' + this.$long + '")'; });
  NumberLong.fromNumber = function (n) { return new NumberLong(n); };
  NumberLong.fromString = function (s) { return new NumberLong(s); };

  function NumberInt(v) {
    if (!new.target) return new NumberInt(v);
    var n = Math.trunc(Number(v === undefined ? 0 : v));
    if (!Number.isFinite(n) || n > 2147483647 || n < -2147483648) throw new TypeError("NumberInt(" + String(v) + "): not a 32-bit integer");
    hidden(this, "$int", n);
  }
  method(NumberInt, "valueOf", function () { return this.$int; });
  method(NumberInt, "toString", function () { return String(this.$int); });
  method(NumberInt, "toJSON", function () { return this.$int; });
  method(NumberInt, "__shell", function () { return 'NumberInt("' + this.$int + '")'; });

  function Double(v) {
    if (!new.target) return new Double(v);
    hidden(this, "$double", Number(v === undefined ? 0 : v));
  }
  method(Double, "valueOf", function () { return this.$double; });
  method(Double, "toString", function () { return String(this.$double); });
  method(Double, "toJSON", function () { return this.$double; });
  method(Double, "__shell", function () { return 'Double("' + String(this.$double) + '")'; });

  function NumberDecimal(v) {
    if (!new.target) return new NumberDecimal(v);
    var s = v instanceof NumberDecimal ? v.$dec : String(v === undefined ? "0" : v).trim();
    hidden(this, "$dec", s);
  }
  method(NumberDecimal, "valueOf", function () { return Number(this.$dec); });
  method(NumberDecimal, "toString", function () { return this.$dec; });
  method(NumberDecimal, "toJSON", function () { return { $numberDecimal: this.$dec }; });
  method(NumberDecimal, "__shell", function () { return "NumberDecimal(" + JSON.stringify(this.$dec) + ")"; });
  NumberDecimal.fromString = function (s) { return new NumberDecimal(s); };

  function Binary(sub, b64) {
    if (!new.target) return new Binary(sub, b64);
    hidden(this, "$sub", Number(sub) | 0);
    hidden(this, "$b64", String(b64 === undefined ? "" : b64));
  }
  method(Binary, "base64", function () { return this.$b64; });
  method(Binary, "subtype", function () { return this.$sub; });
  method(Binary, "toString", function () { return this.$b64; });
  method(Binary, "toJSON", function () { return { $binary: { base64: this.$b64, subType: this.$sub.toString(16).padStart(2, "0") } }; });
  method(Binary, "__shell", function () { return "BinData(" + this.$sub + ", " + JSON.stringify(this.$b64) + ")"; });
  Binary.createFromBase64 = function (b64, sub) { return new Binary(sub || 0, b64); };

  function UUID(hex) {
    if (!new.target) return new UUID(hex);
    var h = (hex === undefined ? native.uuid() : String(hex)).replace(/-/g, "").toLowerCase();
    if (!/^[0-9a-f]{32}$/.test(h)) throw new TypeError('UUID("' + hex + '"): not a UUID');
    hidden(this, "$uuid", h);
  }
  method(UUID, "toString", function () {
    var h = this.$uuid;
    return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" + h.slice(16, 20) + "-" + h.slice(20);
  });
  method(UUID, "toHexString", function () { return this.toString(); });
  method(UUID, "equals", function (o) { return String(o).replace(/-/g, "").toLowerCase() === this.$uuid; });
  method(UUID, "toJSON", function () { return { $uuid: this.toString() }; });
  method(UUID, "__shell", function () { return 'UUID("' + this.toString() + '")'; });

  function Timestamp(t, i) {
    if (!new.target) return new Timestamp(t, i);
    if (t && typeof t === "object") { i = t.i; t = t.t; }
    hidden(this, "$t", Number(t || 0) >>> 0);
    hidden(this, "$i", Number(i || 0) >>> 0);
  }
  method(Timestamp, "getTime", function () { return this.$t; });
  method(Timestamp, "getInc", function () { return this.$i; });
  method(Timestamp, "toString", function () { return "Timestamp(" + this.$t + ", " + this.$i + ")"; });
  method(Timestamp, "toJSON", function () { return { $timestamp: { t: this.$t, i: this.$i } }; });
  method(Timestamp, "__shell", function () { return "Timestamp(" + this.$t + ", " + this.$i + ")"; });

  function Key(name) { hidden(this, "$key", name); }
  method(Key, "toString", function () { return this.$key + "()"; });
  method(Key, "toJSON", function () { var o = {}; o["$" + this.$key[0].toLowerCase() + this.$key.slice(1)] = 1; return o; });
  method(Key, "__shell", function () { return this.$key + "()"; });
  var MIN = new Key("MinKey"), MAX = new Key("MaxKey");
  function MinKey() { return MIN; }
  function MaxKey() { return MAX; }

  function Code(s) {
    if (!new.target) return new Code(s);
    hidden(this, "$code", typeof s === "function" ? s.toString() : String(s === undefined ? "" : s));
  }
  method(Code, "toString", function () { return this.$code; });
  method(Code, "__shell", function () { return "Code(" + JSON.stringify(this.$code) + ")"; });

  // A regular expression JavaScript cannot compile (PCRE's own flags or syntax): kept as text.
  function BSONRegExp(pattern, flags) {
    if (!new.target) return new BSONRegExp(pattern, flags);
    hidden(this, "pattern", String(pattern));
    hidden(this, "flags", String(flags || ""));
  }
  method(BSONRegExp, "toString", function () { return "/" + this.pattern + "/" + this.flags; });
  method(BSONRegExp, "__shell", function () { return "RegExp(" + JSON.stringify(this.pattern) + ", " + JSON.stringify(this.flags) + ")"; });

  function enc(v, depth) {
    if (depth > 150) throw new RangeError("this value nests too deeply (or contains itself)");
    if (v === null) return "null";
    switch (typeof v) {
      case "undefined": return "undefined";
      case "boolean": return v ? "true" : "false";
      case "number":
        if (Number.isNaN(v)) return "NaN";
        if (v === Infinity) return "Infinity";
        if (v === -Infinity) return "-Infinity";
        if (Number.isInteger(v) && !Number.isSafeInteger(v)) return 'Double("' + String(v) + '")';
        return String(v);
      case "bigint": return 'NumberLong("' + v.toString() + '")';
      case "string": return JSON.stringify(v);
      case "symbol": throw new TypeError("a Symbol is not a value MongoDB stores");
      case "function": throw new TypeError("a function is not a value MongoDB stores");
    }
    if (typeof v.__shell === "function") return v.__shell();
    if (v instanceof Date) {
      var ms = v.getTime();
      if (Number.isNaN(ms)) throw new RangeError("Invalid Date");
      return "ISODate(" + ms + ")";
    }
    if (v instanceof RegExp) return "RegExp(" + JSON.stringify(v.source) + ", " + JSON.stringify(v.flags.replace(/[gyd]/g, "")) + ")";
    if (v instanceof Cursor) return enc(v.toArray(), depth);
    if (Array.isArray(v)) return "[" + v.map(function (x) { return enc(x === undefined ? null : x, depth + 1); }).join(", ") + "]";
    if (v instanceof Map) {
      var m = {};
      v.forEach(function (x, k) { m[String(k)] = x; });
      v = m;
    }
    if (v instanceof Number || v instanceof String || v instanceof Boolean) return enc(v.valueOf(), depth);
    var parts = [];
    Object.keys(v).forEach(function (k) {
      var x = v[k];
      if (typeof x === "function") return;
      parts.push(JSON.stringify(k) + ": " + (x === undefined ? "null" : enc(x, depth + 1)));
    });
    return "{" + parts.join(", ") + "}";
  }
  function encode(v) { return enc(v, 0); }

  function decode(text) { return (0, eval)("(" + text + ")"); }

  function args(a) {
    var out = Array.prototype.slice.call(a);
    while (out.length && out[out.length - 1] === undefined) out.pop();
    return out;
  }
  function run(dbName, coll, calls) {
    return decode(native.op(dbName, coll, encode(calls)));
  }

  var BATCH = 1000;
  var REFINE = ["sort", "limit", "skip", "projection", "project", "hint", "collation", "maxTimeMS", "comment", "allowDiskUse", "min", "max", "returnKey", "showRecordId"];
  var IGNORE = ["batchSize", "noCursorTimeout", "readPref", "readConcern", "addOption", "tailable", "maxAwaitTimeMS", "pretty"];

  function Cursor(dbName, coll, calls) {
    hidden(this, "$db", dbName);
    hidden(this, "$coll", coll);
    hidden(this, "$calls", calls);
    hidden(this, "$s", { buf: null, at: 0, skip: 0, more: true, started: false });
  }
  REFINE.forEach(function (m) {
    method(Cursor, m, function () {
      if (this.$s.started) throw new Error(m + "() after the cursor started returning documents");
      this.$calls.push([m, args(arguments)]);
      return this;
    });
  });
  IGNORE.forEach(function (m) { method(Cursor, m, function () { return this; }); });
  method(Cursor, "_fill", function () {
    var s = this.$s;
    s.started = true;
    if ((s.buf === null || s.at >= s.buf.length) && s.more) {
      var r = decode(native.cursor(this.$db, this.$coll, encode(this.$calls), s.skip, BATCH));
      s.buf = r.docs;
      s.at = 0;
      s.skip += r.docs.length;
      s.more = r.more && r.docs.length > 0;
    }
    return s.buf !== null && s.at < s.buf.length;
  });
  method(Cursor, "hasNext", function () { return this._fill(); });
  method(Cursor, "next", function () {
    if (!this._fill()) throw new Error("the cursor has no more documents");
    return this.$s.buf[this.$s.at++];
  });
  method(Cursor, "tryNext", function () { return this._fill() ? this.$s.buf[this.$s.at++] : null; });
  method(Cursor, "isExhausted", function () { return !this._fill(); });
  method(Cursor, "close", function () { this.$s.more = false; this.$s.buf = []; });
  method(Cursor, "toArray", function () {
    var out = [];
    while (this._fill()) out.push(this.$s.buf[this.$s.at++]);
    return out;
  });
  method(Cursor, "forEach", function (f) {
    while (this._fill()) {
      if (f(this.$s.buf[this.$s.at++]) === false) break;
    }
  });
  method(Cursor, "map", function (f) {
    var out = this.toArray().map(function (d) { return f(d); });
    hidden(out, "toArray", function () { return this; });
    hidden(out, "itcount", function () { return this.length; });
    return out;
  });
  method(Cursor, "itcount", function () { var n = 0; while (this._fill()) { this.$s.at++; n++; } return n; });
  method(Cursor, "size", function () { return Number(run(this.$db, this.$coll, this.$calls.concat([["count", []]])).v); });
  method(Cursor, "count", function () { return Number(run(this.$db, this.$coll, this.$calls.concat([["count", []]])).v); });
  method(Cursor, "explain", function (v) { return run(this.$db, this.$coll, this.$calls.concat([["explain", args(arguments)]])).v; });
  method(Cursor, "toString", function () { return "Cursor"; });
  Cursor.prototype[Symbol.iterator] = function* () { while (this._fill()) yield this.$s.buf[this.$s.at++]; };

  function ack(o) { o.acknowledged = true; return o; }
  function writes(r) { return r.detail || {}; }

  function Collection(dbName, name) {
    hidden(this, "$db", dbName);
    hidden(this, "$name", name);
  }
  var C = function (name, f) { method(Collection, name, f); };
  C("_run", function (calls) { return run(this.$db, this.$name, calls); });
  C("_one", function (name, a) { return this._run([[name, args(a)]]); });
  C("getName", function () { return this.$name; });
  C("getFullName", function () { return this.$db + "." + this.$name; });
  C("getDB", function () { return database(this.$db); });
  C("toString", function () { return this.$db + "." + this.$name; });
  C("getCollection", function (n) { return collection(this.$db, this.$name + "." + n); });
  C("find", function () { return new Cursor(this.$db, this.$name, [["find", args(arguments)]]); });
  C("aggregate", function (p) {
    var stages = Array.isArray(p) ? p : Array.prototype.slice.call(arguments).filter(function (s) { return s && typeof s === "object" && !Array.isArray(s) && Object.keys(s).some(function (k) { return k[0] === "$"; }); });
    return new Cursor(this.$db, this.$name, [["aggregate", [stages]]]);
  });
  C("findOne", function () {
    var r = this._one("findOne", arguments);
    return r.docs.length ? r.docs[0] : null;
  });
  C("countDocuments", function () { return Number(this._one("countDocuments", arguments).v); });
  C("count", function () { return Number(this._one("count", arguments).v); });
  C("estimatedDocumentCount", function () { return Number(this._one("estimatedDocumentCount", arguments).v); });
  C("distinct", function () { return this._one("distinct", arguments).v; });
  C("insertOne", function () {
    var r = this._one("insertOne", arguments);
    return ack({ insertedId: writes(r).insertedIds ? writes(r).insertedIds[0] : null });
  });
  function inserted(r) {
    var ids = {};
    (writes(r).insertedIds || []).forEach(function (id, i) { ids[i] = id; });
    return ack({ insertedIds: ids });
  }
  C("insertMany", function () { return inserted(this._one("insertMany", arguments)); });
  C("insert", function () { return inserted(this._one("insert", arguments)); });
  function updated(r) {
    var d = writes(r);
    var up = d.upsertedId !== undefined && d.upsertedId !== null;
    return ack({ insertedId: up ? d.upsertedId : null, matchedCount: Number(d.matchedCount || 0), modifiedCount: Number(d.modifiedCount || 0), upsertedCount: up ? 1 : 0 });
  }
  C("updateOne", function () { return updated(this._one("updateOne", arguments)); });
  C("updateMany", function () { return updated(this._one("updateMany", arguments)); });
  C("update", function () { return updated(this._one("update", arguments)); });
  C("replaceOne", function () { return updated(this._one("replaceOne", arguments)); });
  function deleted(r) { return ack({ deletedCount: r.affected }); }
  C("deleteOne", function () { return deleted(this._one("deleteOne", arguments)); });
  C("deleteMany", function () { return deleted(this._one("deleteMany", arguments)); });
  C("remove", function () { return deleted(this._one("remove", arguments)); });
  ["findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "findAndModify"].forEach(function (m) {
    C(m, function () { var r = this._one(m, arguments); return r.detail === null ? null : r.detail; });
  });
  C("bulkWrite", function () {
    var d = writes(this._one("bulkWrite", arguments));
    return ack({ insertedCount: Number(d.insertedCount || 0), matchedCount: Number(d.matchedCount || 0), modifiedCount: Number(d.modifiedCount || 0), deletedCount: Number(d.deletedCount || 0), upsertedCount: Number(d.upsertedCount || 0) });
  });
  C("createIndex", function () { return this._one("createIndex", arguments).summary.replace(/^index (.*) created$/, "$1"); });
  C("ensureIndex", function () { return this._one("ensureIndex", arguments).summary.replace(/^index (.*) created$/, "$1"); });
  C("createIndexes", function (specs, opts) {
    var self = this;
    return (specs || []).map(function (k) { return self.createIndex(k, opts); });
  });
  C("dropIndex", function () { this._one("dropIndex", arguments); return { ok: 1 }; });
  C("dropIndexes", function () { this._one("dropIndexes", arguments); return { ok: 1 }; });
  C("getIndexes", function () { return this._one("getIndexes", arguments).v; });
  C("indexes", function () { return this._one("getIndexes", arguments).v; });
  C("drop", function () { this._one("drop", arguments); return true; });
  C("renameCollection", function () { this._one("renameCollection", arguments); return { ok: 1 }; });
  C("stats", function () { return this._one("stats", arguments).v; });
  C("exists", function () {
    var self = this;
    return database(this.$db).getCollectionNames().indexOf(self.$name) >= 0;
  });

  ["mapReduce", "watch", "initializeOrderedBulkOp", "initializeUnorderedBulkOp", "hideIndex", "unhideIndex"].forEach(function (m) {
    C(m, function () { throw new Error(m + "() is not supported here" + (m === "mapReduce" ? " (aggregate does it)" : m.indexOf("Bulk") >= 0 ? " (bulkWrite does it)" : "")); });
  });

  function collection(dbName, name) {
    return new Proxy(new Collection(dbName, name), {
      get: function (t, k, r) {
        if (typeof k === "symbol" || k in t) return Reflect.get(t, k, r);
        return collection(dbName, name + "." + k);
      },
    });
  }

  function Database(name) { hidden(this, "$name", name); }
  var D = function (name, f) { method(Database, name, f); };
  D("_one", function (name, a) { return run(this.$name, null, [[name, args(a)]]); });
  D("getName", function () { return this.$name; });
  D("toString", function () { return this.$name; });
  D("getCollection", function (n) { return collection(this.$name, String(n)); });
  D("getSiblingDB", function (n) { return database(String(n)); });
  D("getCollectionNames", function () { return this._one("getCollectionNames", []).v; });
  D("getCollectionInfos", function (filter) {
    var cmd = { listCollections: 1 };
    if (filter) cmd.filter = filter;
    return this.runCommand(cmd).cursor.firstBatch;
  });
  D("runCommand", function (c) { return this._one("runCommand", [c]).v; });
  D("adminCommand", function (c) { return this._one("adminCommand", [c]).v; });
  D("createCollection", function () { this._one("createCollection", arguments); return { ok: 1 }; });
  D("createView", function () { this._one("createView", arguments); return { ok: 1 }; });
  D("dropDatabase", function () { this._one("dropDatabase", []); return { ok: 1, dropped: this.$name }; });
  D("stats", function () { return this._one("stats", []).v; });
  D("aggregate", function (p) { return new Cursor(this.$name, null, [["aggregate", [Array.isArray(p) ? p : Array.prototype.slice.call(arguments)]]]); });
  D("version", function () { return this.adminCommand({ buildInfo: 1 }).version; });
  D("serverBuildInfo", function () { return this.adminCommand({ buildInfo: 1 }); });
  D("serverStatus", function () { return this.adminCommand({ serverStatus: 1 }); });
  D("hostInfo", function () { return this.adminCommand({ hostInfo: 1 }); });
  D("hello", function () { return this.runCommand({ hello: 1 }); });
  D("isMaster", function () { return this.runCommand({ isMaster: 1 }); });
  D("ping", function () { return this.runCommand({ ping: 1 }); });
  D("getUsers", function () { return this.runCommand({ usersInfo: 1 }); });
  D("getRoles", function () { return this.runCommand({ rolesInfo: 1 }); });
  D("currentOp", function () { return { inprog: database("admin").aggregate([{ $currentOp: {} }]).toArray() }; });

  function database(name) {
    return new Proxy(new Database(name), {
      get: function (t, k, r) {
        if (typeof k === "symbol" || k in t) return Reflect.get(t, k, r);
        return t.getCollection(k);
      },
    });
  }

  function show(v) {
    if (typeof v === "string") return v;
    if (v === undefined) return "undefined";
    if (typeof v === "function") return v.toString();
    if (v instanceof Cursor) v = v.toArray();
    try {
      return native.pretty(encode(v));
    } catch (e) {
      return String(v);
    }
  }
  function print() { native.print(Array.prototype.map.call(arguments, show).join(" ")); }
  function printjson(v) { native.print(v === undefined ? "undefined" : show(v)); }
  function printjsononeline(v) { native.print(show(v).replace(/\n\s*/g, " ")); }

  function final(v) {
    if (v instanceof Cursor && !v.$s.started) return "C" + encode({ db: v.$db, coll: v.$coll, calls: v.$calls });
    if (v === undefined) return "U";
    if (typeof v === "string") return "S" + v;
    if (typeof v === "function") return "S" + v.toString();
    if (v instanceof Collection || v instanceof Database) return "S" + v.toString();
    try {
      return "V" + encode(v);
    } catch (e) {
      return "S" + String(v);
    }
  }

  var g = globalThis;
  g.ObjectId = g.ObjectID = ObjectId;
  g.ISODate = ISODate;
  g.NumberLong = g.Long = NumberLong;
  g.NumberInt = g.Int32 = NumberInt;
  g.Double = Double;
  g.NumberDecimal = g.Decimal128 = NumberDecimal;
  g.BinData = g.Binary = Binary;
  g.UUID = UUID;
  g.Timestamp = Timestamp;
  g.MinKey = MinKey;
  g.MaxKey = MaxKey;
  g.Code = Code;
  g.BSONRegExp = BSONRegExp;
  g.print = print;
  g.printjson = printjson;
  g.printjsononeline = printjsononeline;
  g.tojson = show;
  g.sleep = function (ms) { native.sleep(Number(ms) || 0); };
  g.console = { log: print, info: print, warn: print, error: print, debug: print };
  g.EJSON = { stringify: function (v, r, s) { return JSON.stringify(v, r, s); }, parse: function (s) { return JSON.parse(s); } };

  // Values from the server (frozen: a script's own `ObjectId = …` changes nothing here).
  return Object.freeze({
    oid: function (h) { return new ObjectId(h); },
    date: function (ms) { return new Date(ms); },
    long: function (s) { return new NumberLong(s); },
    dec: function (s) { return new NumberDecimal(s); },
    uuid: function (h) { return new UUID(h); },
    bin: function (sub, b64) { return new Binary(sub, b64); },
    ts: function (t, i) { return new Timestamp(t, i); },
    re: function (p, f) {
      if (/^[imsu]*$/.test(f)) {
        try { return new RegExp(p, f); } catch (e) { }
      }
      return new BSONRegExp(p, f);
    },
    code: function (s) { return new Code(s); },
    min: MIN,
    max: MAX,
    database: database,
    final: final,
  });
})();
