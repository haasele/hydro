"""Deterministic pydantic 2.14.1 / pydantic-core 2.50.1 smoke case.

Covers model validation, field aliases, (de)serialisation, JSON schema,
TypeAdapter and direct pydantic-core SchemaValidator usage. Error details are
reduced to error types and locations so messages do not leak environment text.
"""

import pydantic
import pydantic_core
from pydantic import BaseModel, Field, TypeAdapter, ValidationError
from pydantic_core import SchemaValidator, core_schema
from pydantic.json_schema import GenerateJsonSchema

print("pydantic", pydantic.VERSION, "core", pydantic_core.__version__)


class Address(BaseModel):
    city: str
    zip_code: int = Field(alias="zip")


class User(BaseModel):
    id: int
    name: str
    email: str | None = None
    tags: list[str] = []
    address: Address | None = None


user = User(
    id=7,
    name="Ada",
    tags=["admin"],
    address=Address(city="London", zip=12345),
)
print("dump", user.model_dump())
print("dump_alias", user.model_dump(by_alias=True))
print("json", user.model_dump_json())
print("roundtrip", User.model_validate_json(user.model_dump_json(by_alias=True)) == user)

try:
    User(id="x", name=123)
except ValidationError as exc:
    print("errors", len(exc.errors()))
    print("error_types", sorted({error["type"] for error in exc.errors()}))
    print("error_locs", [list(error["loc"]) for error in exc.errors()])


class Point(BaseModel):
    x: float
    y: float


print("coerce", Point(x="1.5", y=2).model_dump())

adapter = TypeAdapter(list[int])
print("adapter", adapter.validate_python(["1", 2, 3]))
print("adapter_json", adapter.validate_json("[4,5]"))

schema = User.model_json_schema(schema_generator=GenerateJsonSchema)
print("schema_keys", sorted(schema.keys()))
print("schema_properties", sorted(schema["properties"].keys()))
print("schema_required", sorted(schema["required"]))

ref_schema = User.model_json_schema(schema_generator=GenerateJsonSchema, ref_template="#/$defs/{model}")
print("defs", sorted(ref_schema.get("$defs", {}).keys()))

validator = SchemaValidator(core_schema.int_schema())
print("core_py", validator.validate_python("42"))
print("core_json", validator.validate_json("7"))

try:
    validator.validate_python("nope")
except pydantic_core.ValidationError as exc:
    print("core_error", exc.error_count(), [error["type"] for error in exc.errors()])

typed = SchemaValidator(
    core_schema.typed_dict_schema(
        {
            "name": core_schema.typed_dict_field(core_schema.str_schema()),
            "age": core_schema.typed_dict_field(
                core_schema.int_schema(), required=False
            ),
        }
    )
)
print("typed_dict", typed.validate_python({"name": "Bob", "age": "3"}))
