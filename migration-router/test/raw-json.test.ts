import { describe, expect, it } from "vitest";
import { rawElements, rawMembers } from "../src/raw-json";

describe("rawMembers", () => {
  it("keeps each member's text, integer-like key order included", () => {
    const text = `{"payload":{"carryover":{"10":1,"9":2}},"close_action":[6, 6]}`;
    const members = rawMembers(text)!;
    expect(members.get("payload")).toBe(`{"carryover":{"10":1,"9":2}}`);
    expect(members.get("close_action")).toBe("[6, 6]");
    expect(JSON.stringify(JSON.parse(members.get("payload")!))).not.toBe(
      members.get("payload"),
    );
  });

  it("reads past strings holding brackets, commas and escaped quotes", () => {
    const text = ` { "a" : "}],{\\"[" , "b":{"c":"\\\\"},"d":-1.5e3,"e":true,"f":null,"g\\u0041":[] } `;
    const members = rawMembers(text)!;
    expect([...members.keys()]).toEqual(["a", "b", "d", "e", "f", "gA"]);
    expect(members.get("a")).toBe(`"}],{\\"["`);
    expect(members.get("b")).toBe(`{"c":"\\\\"}`);
    expect(members.get("d")).toBe("-1.5e3");
    expect(members.get("e")).toBe("true");
    expect(members.get("f")).toBe("null");
    expect(members.get("gA")).toBe("[]");
  });

  it("answers undefined for anything but a JSON object", () => {
    for (const text of ["", "not json{", "[1]", "null", '"s"', "{", '{"a":1}x']) {
      expect(rawMembers(text), text).toBeUndefined();
    }
    expect(rawMembers("{}")).toEqual(new Map());
  });
});

describe("rawElements", () => {
  it("keeps each element's text", () => {
    expect(rawElements(`[ {"notary":"n", "signature":[1,2]} ,"a,]",[[3]],4 ]`)).toEqual([
      `{"notary":"n", "signature":[1,2]}`,
      `"a,]"`,
      "[[3]]",
      "4",
    ]);
    expect(rawElements("[]")).toEqual([]);
  });

  it("answers undefined for anything but a JSON array", () => {
    for (const text of ["", "{}", "[1,", "1"]) {
      expect(rawElements(text), text).toBeUndefined();
    }
  });
});
