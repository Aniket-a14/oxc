export class Accessors {
  get normal(): number { return 1; }
  set normal(value) {}
  get "\uD800"(): string { return "lead"; }
  set "\ud800"(value) {}
  set "\uDC00"(value: boolean) {}
  get "\udc00"() { return true; }
  get "\uD801"(): number { return 1; }
  set "\uD801"(value) {}
  get "\uD800\uDC00"(): bigint { return 1n; }
  set "𐀀"(value) {}
  get "before\uD802after"(): string { return "mixed"; }
  set "before\ud802after"(value) {}
}
