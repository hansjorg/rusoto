extern crate botocore_parser;
extern crate inflector;
extern crate regex;
#[cfg(feature = "serde_codegen")]
extern crate serde_codegen;
extern crate serde_json;
#[cfg(feature = "serde_codegen")]
extern crate syntex;

use std::fs::File;
use std::io::{Read, Write, copy};
use std::path::Path;
use std::process::Command;

use botocore_parser::{Service, Shape};
use inflector::Inflector;

pub struct AmazonService {
    name: String,
    type_name: String,
    protocol_date: String
}

pub fn generate(
    service: AmazonService,
    botocore_path: &Path,
    base_destination: &Path) {
    let botocore_destination = base_destination.join(format!("{}_botocore.rs", service.name));
    let serde_destination = base_destination.join(format!("{}.rs", service.name));
    let input_location = botocore_path.join(format!("{}/{}/service-2.json", service.name, service.protocol_date));

    let input = input_location.to_str().expect(&format!("Invalid service definition path for {} {}", service.protocol_date, service.name));

    botocore_generate(input, &service.type_name, botocore_destination.as_path());
    serde_generate(botocore_destination.as_path(), serde_destination.as_path());
}

fn botocore_generate(input: &str, type_name: &str, destination: &Path) {
    let mut f = File::open(input).unwrap();
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);

    let service: Service = serde_json::from_str(&s).expect("Invalid botocore input");

    if &service.metadata.protocol == "query" {
        return python_generate(input, type_name, destination);
    }

    let mut source = String::new();
    let error_type_name = error_type(type_name);

    source.push_str("use std::io::Read;\n");

    match &service.metadata.protocol[..] {
        "json" => {
            source.push_str(&format!("
use std::result;

use serde_json;

use credentials::ProvideAWSCredentials;
use error::AWSError;
use regions::Region;
use signature::SignedRequest;

#[derive(Debug, Default, Deserialize)]
pub struct {error_type_name} {{
    __type: String,
    message: String,
}}

pub type Result<T> = result::Result<T, {error_type_name}>;

impl From<AWSError> for {error_type_name} {{
    fn from(err: AWSError) -> Self {{
        let AWSError(message) = err;

        {error_type_name} {{
            __type: \"Unknown\".to_string(),
            message: message.to_string(),
        }}
    }}
}}

fn parse_error(body: &str) -> {error_type_name} {{
    if let Ok(decoded) = serde_json::from_str::<{error_type_name}>(&body) {{
        decoded
    }} else {{
        {error_type_name} {{
            __type: \"DecodeError\".to_string(),
            message: body.to_string(),
        }}
    }}
}}\n",
                error_type_name = error_type_name,
            ));
        }
        "rest-xml" => {
            source.push_str("
use hyper::header::Headers;
use xml::reader::EventReader;

use credentials::ProvideAWSCredentials;
use error::AWSError;
use regions::Region;
use signature::SignedRequest;
use xmlutil::{XmlResponseFromAws, XmlParseError, Next, Peek};

#[derive(Debug, PartialEq)]
pub enum ArgumentLocation {
    Header,
    Body,
    Headers,
    Querystring,
    Uri,
}\n");
        },
        _ => {},
    }

    // generate rust structs for the botocore shapes
    source.push_str(&render_shapes(&service));

    // generate the service client struct
    source.push_str(&format!("pub struct {}<'a> {{", type_name));
    source.push_str("\tcreds: Box<ProvideAWSCredentials + 'a>,");
    source.push_str("\tregion: &'a Region");
    source.push_str("}\n");

    // implement each botocore operation as function for the client
    source.push_str(&format!("impl<'a> {}<'a> {{ ", type_name));
    source.push_str(&format!("\tpub fn new<P: ProvideAWSCredentials + 'a>(creds: P, region: &'a Region) -> {}<'a> {{", type_name));
    source.push_str(&format!("\t\t{} {{ creds: Box::new(creds), region: region }}", type_name));
    source.push_str("\t}");

    // each protocol type will require operations performed in different ways
    let operations = match &*service.metadata.protocol {
        //"rest-xml" => rest_xml_operations(&service),
        "json" => json_operations(&service),
        _ => panic!(format!("Unknown protocol type '{}'", service.metadata.protocol))
    };
    source.push_str(&operations);
    source.push_str("}");

    let mut outfile = File::create(destination).expect("couldn't open file for writing");
    let _ = outfile.write_all(source.as_bytes());
}

fn python_generate(input: &str, type_name: &str, destination: &Path) {
    let mut command = Command::new("codegen/botocore_parser.py");

    command.args(&[input, type_name]);

    let output = command.output().expect("couldn't get output of child process");

    if !output.status.success() {
        println!("{}", String::from_utf8_lossy(&output.stdout[..]));
        println!("{}", String::from_utf8_lossy(&output.stderr[..]));
        panic!("child process was unsuccessful");
    }

    let mut file = File::create(destination).expect("couldn't open file for writing");
    copy(&mut &output.stdout[..], &mut file).expect("failed to write generated code to file");
}

#[cfg(feature = "serde_codegen")]
fn serde_generate(source: &Path, destination: &Path) {
    let mut registry = syntex::Registry::new();

    serde_codegen::register(&mut registry);
    registry.expand("", source, destination).expect("failed to generate code with Serde");
}

#[cfg(not(feature = "serde_codegen"))]
fn serde_generate(_source: &Path, _destination: &Path) {
}

impl AmazonService {
    pub fn new<S: ToString>(name: S, type_name: S, protocol_date: S) -> AmazonService {
        AmazonService { name: name.to_string(), type_name: type_name.to_string(), protocol_date: protocol_date.to_string() }
    }
}

fn print_docs_for_operation(op: &botocore_parser::Operation) -> String {
    let mut doc_string = String::new();
    match op.documentation {
        None => (),
        Some(ref docs) => doc_string.push_str(&format!("\t// {}\n", docs)),
    }
    match op.documentationUrl {
        None => (),
        Some(ref doc_uri) => doc_string.push_str(&format!("\t// {}\n", doc_uri)),
    }
    doc_string
}

// Translate botocore operations to Rust functions for json services like DynamoDB and KMS
fn json_operations(service: &Service) -> String {
    let mut src = String::new();

    let target_prefix = service.metadata.targetPrefix.as_ref().expect("targetPrefix not defined for json protocol operation");

    for operation in service.operations.values() {
        src.push_str(&print_docs_for_operation(operation));
        let output_shape = operation.output_shape_or("()");

        src.push_str(&format!("\tpub fn {}(&mut self, input: &{}) -> Result<{}> {{\n", operation.name.to_snake_case(), operation.input_shape(), output_shape));
        src.push_str("\t\tlet encoded = serde_json::to_string(&input).unwrap();\n");
        src.push_str(&format!("\t\tlet mut request = SignedRequest::new(\"{}\", \"{}\", &self.region, \"{}\");\n", operation.http.method, service.metadata.endpointPrefix, operation.http.requestUri));
        src.push_str("\t\trequest.set_content_type(\"application/x-amz-json-1.0\".to_string());\n");
        src.push_str(&format!("\t\trequest.add_header(\"x-amz-target\", \"{}.{}\");\n", target_prefix, operation.name));
        src.push_str("\t\trequest.set_payload(Some(encoded.as_bytes()));\n");
        src.push_str("\t\tlet mut result = request.sign_and_execute(try!(self.creds.credentials()));\n");
        src.push_str("\t\tlet status = result.status.to_u16();\n");
        src.push_str("\t\tlet mut body = String::new();\n");
        src.push_str("\t\tresult.read_to_string(&mut body).unwrap();\n");
        src.push_str("\t\tmatch status {\n");
        src.push_str("\t\t\t200 => {\n");

        if operation.output.is_some() {
        src.push_str(&format!("\t\t\t\tlet decoded: {} = serde_json::from_str(&body).unwrap();\n", output_shape));
        } else {
            src.push_str("\t\t\t\tlet decoded = ();\n");
        }

        src.push_str("\t\t\t\tOk(decoded)\n");
        src.push_str("\t\t\t}\n");
        src.push_str("\t\t\t_ => {\n");
        src.push_str("\t\t\t\tErr(parse_error(&body))\n");
        src.push_str("\t\t\t}\n");
        src.push_str("\t\t}\n");
        src.push_str("\t}\n");
    }
    src
}

// Translate botocore "shapes" to Rust types
fn render_shapes(service: &Service) -> String {
    let mut src = String::new();
    for (name, shape) in service.shapes.iter() {
        // String is already a type in Rust
        if name == "String" {
            continue;
        }
        if shape.shape_type == "structure" {
            src = src + &format!("{}\n", struct_type(name, &shape));
        } else {
            let rust_type = match &*shape.shape_type {
                "structure" => struct_type(name, &shape),
                "map" => format!("::std::collections::HashMap<{},{}>", shape.key(), shape.value()),
                "list" => format!("Vec<{}>", shape.member()),
                _ => primitive_type(&shape.shape_type)
            };
            src = src + &format!("pub type {} = {};\n", name, rust_type);
        }
    }
    src
}


fn struct_type(name: &str, shape: &Shape) -> String {
    if shape.members.is_empty() {
        return format!("#[derive(Debug, Serialize, Deserialize, Default)]\npub struct {};", name);
    }
    let mut struct_type = format!("#[derive(Debug, Serialize, Deserialize, Default)]\npub struct {} {{\n", name);
    for (member_name, member) in shape.members.iter() {
        if member.documentation.is_some() {
            //struct_type = struct_type + "\t// documentation\n";
        }
        if shape.required(member_name) {
            struct_type = struct_type + &format!("\tpub {}: {},\n", member_name, member.shape);
        } else {
            // There's surely a better way to do this:
            if member_name == "type" {
                struct_type = struct_type + &format!("\tpub aws_{}: Option<{}>,\n", member_name, member.shape)
            }
            else {
                struct_type = struct_type + &format!("\tpub {}: Option<{}>,\n", member_name, member.shape)
            }
        }
    }
    struct_type = struct_type + "}\n";
    struct_type
}

fn primitive_type(shape_type: &str) -> String {
    match shape_type {
        "string" => "String".to_string(),
        "integer" => "i32".to_string(),
        "long" => "i64".to_string(),
        "float" => "f32".to_string(),
        "double" => "f64".to_string(),
        "blob" => "Vec<u8>".to_string(),
        "boolean" => "bool".to_string(),
        // yes, this is a float type for a timestmap.
        // that's how it comes back from AWS
        "timestamp" => "f64".to_string(),
        _ => panic!(format!("Unknown type '{}'", shape_type))
    }
}

fn error_type(client_type_name: &str) -> &'static str {
    match client_type_name {
        "DynamoDBClient" => "DynamoDBError",
        "KMSClient" => "KMSError",
        "ECSClient" => "ECSError",
        "SQSClient" => "SQSError",
        _ => panic!("Unknown client type."),
    }
}
