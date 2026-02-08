use rusqlite::Connection;
use serde_json::{json, Value};
use tree_sitter::{Parser, Point, Node};
use std::collections::HashMap;

// 補完ロジックのメインエントリー
pub fn process_completion(
    conn: &Connection,
    content: &str,
    line: u32,
    character: u32,
    _file_path: Option<String>,
) -> anyhow::Result<Value> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    parser.set_language(&language)?;

    let tree = parser.parse(content, None).ok_or_else(|| anyhow::anyhow!("Failed to parse content"))?;
    let root = tree.root_node();
    
    // カーソル位置 (row, col)
    // 補完のリクエストは、通常ドットやアローの直後に来る
    // 例: "obj.|" -> cursor is after dot
    // 前の文字を確認するために、行を取得する必要があるが、tree-sitterのみで解決を試みる
    
    let row = line as usize;
    let col = character as usize;
    
    // カーソル位置の直前のノードを取得したい
    // descendant_for_point_range は範囲内のノードを返すが、
    // "obj." の直後の場合、dot node か、あるいは次の空ノードになる可能性がある
    
    let point = Point::new(row, if col > 0 { col - 1 } else { 0 });
    let node = root.descendant_for_point_range(point, point);

    if let Some(n) = node {
        // ドットまたはアロー演算子かどうかチェック
        let node_type = n.kind();
        if node_type == "." || node_type == "->" || node_type == "::" {
            // 左側の式を取得 (field_expression の object)
            // field_expression ( . or -> )
            // qualified_identifier ( :: )
            
            if let Some(parent) = n.parent() {
                let p_kind = parent.kind();
                if p_kind == "field_expression" {
                    if let Some(obj_node) = parent.child_by_field_name("argument") {
                        let var_name = obj_node.utf8_text(content.as_bytes())?;
                        return resolve_and_fetch_members(conn, var_name, &root, content, row, node_type == "->");
                    }
                } else if p_kind == "qualified_identifier" {
                    if let Some(scope_node) = parent.child_by_field_name("scope") {
                        let scope_name = scope_node.utf8_text(content.as_bytes())?;
                        return resolve_static_members(conn, scope_name);
                    }
                }
            }
        } else {
            // カーソルが識別子の上にある場合 (入力途中: "obj.Me|")
            // 親を辿って field_expression を探す
            let mut parent = n.parent();
            while let Some(p) = parent {
                if p.kind() == "field_expression" {
                    if let Some(obj_node) = p.child_by_field_name("argument") {
                        let var_name = obj_node.utf8_text(content.as_bytes())?;
                        return resolve_and_fetch_members(conn, var_name, &root, content, row, false); // is_arrow判定は簡易
                    }
                    break;
                } else if p.kind() == "qualified_identifier" {
                    if let Some(scope_node) = p.child_by_field_name("scope") {
                        let scope_name = scope_node.utf8_text(content.as_bytes())?;
                        return resolve_static_members(conn, scope_name);
                    }
                    break;
                }
                parent = p.parent();
            }
        }
    }

    Ok(json!([]))
}

fn resolve_and_fetch_members(
    conn: &Connection,
    var_name: &str,
    root: &Node,
    content: &str,
    cursor_row: usize,
    _is_arrow: bool,
) -> anyhow::Result<Value> {
    // 1. 変数の型を推論
    let type_name = infer_variable_type(var_name, root, content, cursor_row)?;
    
    if let Some(mut t_name) = type_name {
        // 2. typedef 解決 (再帰的に行うべきだが、1段階だけやる)
        // FTransform -> UE::Math::TTransform
        t_name = resolve_typedef(conn, &t_name)?;
        
        // 3. メンバ取得
        // struct/class の区別なく検索
        let members = fetch_members_recursive(conn, &t_name)?;
        return Ok(json!(members));
    }

    Ok(json!([]))
}

fn resolve_static_members(conn: &Connection, scope_name: &str) -> anyhow::Result<Value> {
    // 1. typedef 解決
    let t_name = resolve_typedef(conn, scope_name)?;
    
    // 2. 静的メンバとEnum値を取得
    let members = fetch_members_recursive(conn, &t_name)?;
    // TODO: フィルタリング (is_static == 1 or enum_item)
    
    Ok(json!(members))
}

fn resolve_typedef(conn: &Connection, type_name: &str) -> anyhow::Result<String> {
    let mut stmt = conn.prepare("SELECT base_class FROM classes WHERE name = ? AND symbol_type = 'typedef' LIMIT 1")?;
    let mut rows = stmt.query([type_name])?;
    
    if let Some(row) = rows.next()? {
        if let Some(base) = row.get::<_, Option<String>>(0)? {
            // UE::Math::TTransform<double> -> TTransform を抽出
            // 簡易的に最後の単語を取得
            // "UE::Math::TTransform" -> "TTransform"
            // "TTransform<double>" -> "TTransform"
            
            // TODO: もっと真面目なパース
            if let Some(last_part) = base.split("::").last() {
                let clean_name = last_part.split('<').next().unwrap_or(last_part);
                return Ok(clean_name.to_string());
            }
        }
    }
    Ok(type_name.to_string())
}

fn fetch_members_recursive(conn: &Connection, class_name: &str) -> anyhow::Result<Vec<Value>> {
    // query.rs のロジックを再利用したいが、関数分割されていないので再実装するか
    // query::process_query 内の GetClassMembersRecursive を呼び出せればベスト
    // ここでは簡易実装
    
    let mut result = Vec::new();
    let mut queue = vec![class_name.to_string()];
    let mut visited = HashMap::new();
    
    while let Some(current) = queue.pop() {
        if visited.contains_key(&current) { continue; }
        visited.insert(current.clone(), true);
        
        // クラスID取得
        let mut stmt = conn.prepare("SELECT id FROM classes WHERE name = ? LIMIT 1")?;
        let mut rows = stmt.query([&current])?;
        
        if let Some(row) = rows.next()? {
            let class_id: i64 = row.get(0)?;
            
            // メンバ取得
            let mut mem_stmt = conn.prepare(
                "SELECT name, type, return_type, access, is_static, detail FROM members WHERE class_id = ?"
            )?;
            let mem_rows = mem_stmt.query_map([class_id], |row| {
                Ok(json!({
                    "label": row.get::<_, String>(0)?,
                    "kind": map_kind(row.get::<_, String>(1)?.as_str()),
                    "detail": row.get::<_, Option<String>>(2)?, // return_type -> detail
                    "documentation": row.get::<_, Option<String>>(5)?,
                    "insertText": row.get::<_, String>(0)?,
                }))
            })?;
            
            for m in mem_rows {
                result.push(m?);
            }
            
            // 親クラス取得
            let mut parent_stmt = conn.prepare("SELECT parent_name FROM inheritance WHERE child_id = ?")?;
            let p_rows = parent_stmt.query_map([class_id], |row| Ok(row.get::<_, String>(0)?))?;
            for p in p_rows {
                queue.push(p?);
            }
        }
    }
    
    Ok(result)
}

fn map_kind(k: &str) -> i64 {
    match k {
        "function" => 2, // Method
        "variable" | "property" => 5, // Field
        "enum_item" => 20, // EnumMember
        _ => 1, // Text
    }
}

// 簡易型推論 (Luaロジックの移植)
fn infer_variable_type(_var_name: &str, _root: &Node, _content: &str, _cursor_row: usize) -> anyhow::Result<Option<String>> {
    // Tree-sitter query to find declaration
    // 実際にはクエリカーソルを使う必要があるが、ここでは簡易的に実装
    // "declarator" フィールドを持つノードを探す
    
    // TODO: query.rs から QUERY_STR を持ってくるか、ここで定義するか
    // ここでは単純なスキャンを行う (本当はQueryを使うべき)
    
    // 暫定: 常に None (後で実装)
    Ok(None)
}
