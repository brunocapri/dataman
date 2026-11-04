// the role: what the agent may do
db.getSiblingDB("admin").runCommand({"createRole":"dataman_agent_a1b2c3d4","privileges":[{"resource":{"db":"shop","collection":"orders"},"actions":["find","listIndexes","collStats","insert","update","remove"]},{"resource":{"db":"shop","collection":"dataman_agent_a1b2c3d4_views.users"},"actions":["find"]},{"resource":{"db":"shop","collection":"users"},"actions":["insert","update","remove"]}],"roles":[]})

// hidden or masked fields: views the agent reads instead
db.getSiblingDB("shop").runCommand({"create":"dataman_agent_a1b2c3d4_views.users","viewOn":"users","pipeline":[{"$project":{"password_hash":0}},{"$set":{"email":{"$cond":[{"$eq":[{"$type":"$email"},"missing"]},"$$REMOVE",{"$cond":[{"$eq":["$email",null]},null,{"$let":{"vars":{"s":{"$convert":{"input":"$email","to":"string","onError":"","onNull":""}}},"in":{"$cond":[{"$gt":[{"$strLenCP":"$$s"},4]},{"$concat":[{"$substrCP":[{"$reduce":{"input":{"$range":[0,{"$toInt":{"$ceil":{"$divide":[{"$subtract":[{"$strLenCP":"$$s"},4]},64]}}}]},"initialValue":"","in":{"$concat":["$$value","••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••"]}}},0,{"$subtract":[{"$strLenCP":"$$s"},4]}]},{"$substrCP":["$$s",{"$subtract":[{"$strLenCP":"$$s"},4]},4]}]},{"$substrCP":[{"$reduce":{"input":{"$range":[0,{"$toInt":{"$ceil":{"$divide":[{"$strLenCP":"$$s"},64]}}}]},"initialValue":"","in":{"$concat":["$$value","••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••••"]}}},0,{"$strLenCP":"$$s"}]}]}}}]}]}}}]})

// the login, last (MongoDB hashes the password; the copied script holds it)
db.getSiblingDB("admin").runCommand({"createUser":"dataman_agent_a1b2c3d4","pwd":"<password from the Keychain>","roles":[{"role":"dataman_agent_a1b2c3d4","db":"admin"}],"mechanisms":["SCRAM-SHA-256"],"customData":{"dataman":"dataman-managed v1","conn":"a1b2c3d4e5f6"}})

// hidden: shop.audit_log → no privilege at all
